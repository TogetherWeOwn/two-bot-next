//! Read-only automod verdict preview (docs/automod-preview-spec.md, slice 1).
//!
//! Answers, for one synthetic message: which single automod filter would
//! decide it under the given config — or none. First-match order, exemption
//! classes, and deciding-knob *names* mirror the live matcher exactly; the
//! verdict carries no threshold values, counts, excerpts, or digests.
//!
//! The repeat check is read-only against the caller's live history: it can
//! only ever read the synthetic author's own rows (the tracker key is
//! `guild:author` and the author is the invoker). Nothing is recorded, so a
//! preview can neither build nor break a real streak.

use crate::automod::{
    has_bad_word, has_blocked_attachment, has_external_link, has_invite, normalize_content,
    AutomodConfig, AutomodFilter, AutomodMessage, RepeatTracker, ZERO_WIDTH,
};

/// Exemption class for an exempt verdict: the invoker's own channel or
/// roles. Channel wins when both apply, matching the live exemption order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewExemption {
    ExemptChannel,
    BypassRole,
}

/// Verdict result. `Partial` is the spec §5.1/T19 fallback shape for an
/// unproven repeat evaluation: no deciding filter or knob, later filters
/// not evaluated. The v1 read-only path proves CREATE-equivalence (T2), so
/// the core does not emit it; the handler reserves it for a future path
/// that cannot prove equivalence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewResult {
    Matched,
    NoMatch,
    Exempt,
    Partial,
}

/// First-match preview verdict. Field inventory (pinned by test): enums, a
/// static knob-name slice, and the mode bit only — never a threshold value,
/// count, matched substring, host, extension, list size, or digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewVerdict {
    pub result: PreviewResult,
    pub filter: Option<AutomodFilter>,
    pub knobs: &'static [&'static str],
    pub exemption: Option<PreviewExemption>,
    pub dry_run: bool,
}

impl PreviewVerdict {
    /// T19 fallback constructor: repeat evaluation indeterminate, so no
    /// complete first-match verdict is available.
    #[must_use]
    pub fn partial(dry_run: bool) -> Self {
        Self {
            result: PreviewResult::Partial,
            filter: None,
            knobs: &[],
            exemption: None,
            dry_run,
        }
    }
}

/// Deciding setting *names* for a matched filter (spec §3.2 knob table).
/// `invite_link` is a fixed rule with no setting.
#[must_use]
pub fn deciding_knobs(filter: AutomodFilter) -> &'static [&'static str] {
    match filter {
        AutomodFilter::BadWords => &["TWO_AUTOMOD_BAD_WORDS"],
        AutomodFilter::RepeatedMessage => &[
            "TWO_AUTOMOD_REPEAT_COUNT",
            "TWO_AUTOMOD_REPEAT_WINDOW_SECONDS",
        ],
        AutomodFilter::MentionSpam => &["TWO_AUTOMOD_MENTION_LIMIT"],
        AutomodFilter::InviteLink => &[],
        AutomodFilter::ExternalLink => &["TWO_AUTOMOD_ALLOWED_DOMAINS"],
        AutomodFilter::AttachmentType => &["TWO_AUTOMOD_BLOCKED_ATTACHMENT_EXTENSIONS"],
    }
}

/// Setting *name* behind an exemption class (spec §3.2 knob table).
#[must_use]
pub fn exemption_knob(exemption: PreviewExemption) -> &'static str {
    match exemption {
        PreviewExemption::ExemptChannel => "TWO_AUTOMOD_EXEMPT_CHANNEL_IDS",
        PreviewExemption::BypassRole => "TWO_AUTOMOD_BYPASS_ROLE_IDS",
    }
}

/// Setting names behind an exemption class, as a verdict-ready slice.
#[must_use]
pub fn exemption_knobs(exemption: PreviewExemption) -> &'static [&'static str] {
    match exemption {
        PreviewExemption::ExemptChannel => &["TWO_AUTOMOD_EXEMPT_CHANNEL_IDS"],
        PreviewExemption::BypassRole => &["TWO_AUTOMOD_BYPASS_ROLE_IDS"],
    }
}

fn preview_exemption(
    policy: &crate::automod::AutomodPolicy,
    message: &AutomodMessage,
) -> Option<PreviewExemption> {
    if policy.exempt_channel_ids.contains(&message.channel_id) {
        return Some(PreviewExemption::ExemptChannel);
    }
    if message
        .role_ids
        .iter()
        .any(|role| policy.bypass_role_ids.contains(role))
    {
        return Some(PreviewExemption::BypassRole);
    }
    None
}

/// Evaluate one synthetic message against `config` without recording
/// anything. The caller builds `message` with the invoker's own ids,
/// `author_is_bot: false`, a `preview:<interaction_id>` message id, and
/// the receipt timestamp; `mention_count` is the synthetic explicit-mention
/// count (compared directly — no snowflake ids are fabricated).
/// Attachment names ride on the message, as in live delivery.
#[must_use]
pub fn preview_automod(
    message: &AutomodMessage,
    mention_count: usize,
    config: &AutomodConfig,
    repeats: &RepeatTracker,
) -> PreviewVerdict {
    let matched = |filter: AutomodFilter| PreviewVerdict {
        result: PreviewResult::Matched,
        filter: Some(filter),
        knobs: deciding_knobs(filter),
        exemption: None,
        dry_run: config.dry_run,
    };
    if let Some(exemption) = preview_exemption(&config.policy, message) {
        return PreviewVerdict {
            result: PreviewResult::Exempt,
            filter: None,
            knobs: exemption_knobs(exemption),
            exemption: Some(exemption),
            dry_run: config.dry_run,
        };
    }
    let normalized = normalize_content(&message.content);
    if has_bad_word(&normalized, &config.policy.bad_words) {
        return matched(AutomodFilter::BadWords);
    }
    if repeats.preview_would_match(message, &normalized, &config.policy) {
        return matched(AutomodFilter::RepeatedMessage);
    }
    if mention_count >= config.policy.mention_limit {
        return matched(AutomodFilter::MentionSpam);
    }
    let link_content: String = normalized
        .chars()
        .filter(|c| !ZERO_WIDTH.contains(c))
        .collect();
    if has_invite(&link_content) {
        return matched(AutomodFilter::InviteLink);
    }
    if has_external_link(&link_content, &config.policy.allowed_domains) {
        return matched(AutomodFilter::ExternalLink);
    }
    if has_blocked_attachment(
        &message.attachment_names,
        &config.policy.blocked_attachment_extensions,
    ) {
        return matched(AutomodFilter::AttachmentType);
    }
    PreviewVerdict {
        result: PreviewResult::NoMatch,
        filter: None,
        knobs: &[],
        exemption: None,
        dry_run: config.dry_run,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(content: &str) -> AutomodMessage {
        AutomodMessage {
            guild_id: "preview-guild".to_owned(),
            channel_id: "preview-channel".to_owned(),
            message_id: "preview:interaction-1".to_owned(),
            author_id: "preview-author".to_owned(),
            author_is_bot: false,
            role_ids: vec![],
            content: content.to_owned(),
            mentioned_user_ids: vec![],
            attachment_names: vec![],
            observed_timestamp_ms: 1_000_000,
        }
    }

    #[test]
    fn knob_table_covers_every_filter() {
        for filter in [
            AutomodFilter::BadWords,
            AutomodFilter::RepeatedMessage,
            AutomodFilter::MentionSpam,
            AutomodFilter::InviteLink,
            AutomodFilter::ExternalLink,
            AutomodFilter::AttachmentType,
        ] {
            let knobs = deciding_knobs(filter);
            if filter == AutomodFilter::InviteLink {
                assert!(knobs.is_empty(), "invite_link is a fixed rule");
            } else {
                assert!(!knobs.is_empty(), "{filter:?} needs a knob row");
            }
            for knob in knobs {
                assert!(
                    knob.starts_with("TWO_AUTOMOD_"),
                    "knob is a setting name, never a value: {knob}"
                );
            }
        }
    }

    #[test]
    fn verdict_shape_carries_no_values() {
        let policy = crate::automod::AutomodPolicy {
            bad_words: vec!["bannedword".to_owned()],
            ..crate::automod::AutomodPolicy::default()
        };
        let config = AutomodConfig {
            enabled: true,
            dry_run: true,
            policy,
        };
        let repeats = RepeatTracker::default();
        let verdict = preview_automod(&message("this has bannedword in it"), 0, &config, &repeats);
        assert_eq!(verdict.filter, Some(AutomodFilter::BadWords));
        let rendered = format!("{verdict:?}");
        assert!(
            !rendered.contains("bannedword"),
            "no matched token: {rendered}"
        );
        assert!(!rendered.contains("this has"), "no input echo: {rendered}");
    }

    #[test]
    fn partial_shape_has_no_filter_or_knob() {
        for dry_run in [true, false] {
            let verdict = PreviewVerdict::partial(dry_run);
            assert_eq!(verdict.result, PreviewResult::Partial);
            assert_eq!(verdict.filter, None);
            assert!(verdict.knobs.is_empty());
            assert_eq!(verdict.exemption, None);
            assert_eq!(verdict.dry_run, dry_run);
        }
    }
}
