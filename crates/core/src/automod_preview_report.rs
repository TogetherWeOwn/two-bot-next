//! Verdict reply templates plus content-free audit/metric builders for
//! `/automod-preview` (docs/automod-preview-spec.md §§3.2, 6; slice 3b).
//!
//! Pure core: no I/O, no clock, no config read. Every builder takes enums
//! (and a pre-rendered UTC timestamp) so the reply and audit row cannot
//! carry input text, threshold values, counts, excerpts, or digests by
//! construction — the caller's signature has no content parameter to leak.
//!
//! The handler (later slice) owns strict decode, scope checks, the limiter,
//! the live-snapshot refresh, and the transaction; it calls into these
//! builders to render and to fill the `automation_audit_log` row and the
//! fixed-label metric counter.

use crate::automod::AutomodFilter;
use crate::automod_preview::{PreviewExemption, PreviewResult, PreviewVerdict};

/// Input-length bucket (UTF-16 units) for the audit `reason` field.
/// A length, never content.
#[must_use]
pub fn preview_length_bucket(len_utf16: u32) -> &'static str {
    if len_utf16 <= 50 {
        "0_50"
    } else if len_utf16 <= 500 {
        "51_500"
    } else {
        "501_2000"
    }
}

/// `outcome` column value for an evaluated preview.
#[must_use]
pub fn preview_audit_outcome(verdict: &PreviewVerdict) -> &'static str {
    match verdict.result {
        PreviewResult::Matched => "matched",
        PreviewResult::NoMatch => "no_match",
        PreviewResult::Exempt => "exempt",
        PreviewResult::Partial => "partial",
    }
}

/// `reason` column value for an evaluated preview, built only from enums:
/// `filter=<name|none>;exemption=<class|none>;mode=<dry_run|enforce>;len=<bucket>`.
/// `exempt` stores `filter=none` plus the selected exemption class; every
/// other outcome stores `exemption=none`. `partial` always stores
/// `filter=none` (repeat evaluation was `indeterminate`).
#[must_use]
pub fn audit_reason_for_verdict(verdict: &PreviewVerdict, len_utf16: u32) -> String {
    let filter = match verdict.result {
        PreviewResult::Matched => verdict.filter.map_or("none", AutomodFilter::as_str),
        PreviewResult::NoMatch | PreviewResult::Exempt | PreviewResult::Partial => "none",
    };
    let exemption = match verdict.exemption {
        Some(PreviewExemption::ExemptChannel) => "exempt_channel",
        Some(PreviewExemption::BypassRole) => "bypass_role",
        None => "none",
    };
    let mode = if verdict.dry_run {
        "dry_run"
    } else {
        "enforce"
    };
    format!(
        "filter={filter};exemption={exemption};mode={mode};len={}",
        preview_length_bucket(len_utf16)
    )
}

/// `reason` for an in-scope rate refusal: nothing was evaluated, so
/// `filter=none;exemption=none`; the effective mode and input-length bucket
/// are still recorded.
#[must_use]
pub fn audit_reason_rate_limited(dry_run: bool, len_utf16: u32) -> String {
    let mode = if dry_run { "dry_run" } else { "enforce" };
    format!(
        "filter=none;exemption=none;mode={mode};len={}",
        preview_length_bucket(len_utf16)
    )
}

/// Fixed-label metric counter for one preview event. Every field is drawn
/// from the spec's closed vocabularies (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewMetricLabels {
    /// `matched|no_match|exempt|partial|rate_limited|not_evaluated|unavailable`
    pub result: &'static str,
    /// one of the six filter names, or `none`
    pub filter: &'static str,
    /// `dry_run|enforce|not_evaluated` (`not_evaluated` = no effective snapshot)
    pub mode: &'static str,
}

/// Metric labels for an evaluated verdict.
#[must_use]
pub fn metric_for_verdict(verdict: &PreviewVerdict) -> PreviewMetricLabels {
    PreviewMetricLabels {
        result: preview_audit_outcome(verdict),
        filter: match (verdict.result, verdict.filter) {
            (PreviewResult::Matched, Some(filter)) => filter.as_str(),
            _ => "none",
        },
        mode: if verdict.dry_run {
            "dry_run"
        } else {
            "enforce"
        },
    }
}

/// Metric labels for an in-scope rate refusal.
#[must_use]
pub fn metric_rate_limited(dry_run: bool) -> PreviewMetricLabels {
    PreviewMetricLabels {
        result: "rate_limited",
        filter: "none",
        mode: if dry_run { "dry_run" } else { "enforce" },
    }
}

/// Metric labels for an out-of-scope/disabled/DM attempt: no audit row, no
/// effective mode.
#[must_use]
pub fn metric_not_evaluated() -> PreviewMetricLabels {
    PreviewMetricLabels {
        result: "not_evaluated",
        filter: "none",
        mode: "not_evaluated",
    }
}

/// Metric labels when the verdict is suppressed (audit failure, timeout,
/// abandoned claim): the effective mode if a snapshot exists, else the
/// `not_evaluated` sentinel.
#[must_use]
pub fn metric_unavailable(dry_run: Option<bool>) -> PreviewMetricLabels {
    PreviewMetricLabels {
        result: "unavailable",
        filter: "none",
        mode: match dry_run {
            Some(true) => "dry_run",
            Some(false) => "enforce",
            None => "not_evaluated",
        },
    }
}

/// Derive the exemption knob *name* from the persisted exemption class for
/// redelivery replay (§6). Never consults current config or member roles.
/// A missing or invalid stored class returns `None`: replay fails closed as
/// unavailable, never a guessed class or a fresh evaluation.
#[must_use]
pub fn replay_exemption_knob(stored_class: &str) -> Option<&'static str> {
    match stored_class {
        "exempt_channel" => Some("TWO_AUTOMOD_EXEMPT_CHANNEL_IDS"),
        "bypass_role" => Some("TWO_AUTOMOD_BYPASS_ROLE_IDS"),
        _ => None,
    }
}

/// Canonical `current at <UTC time>` rendering for one committed receipt
/// timestamp (receipt millis since epoch). The first reply and every replay
/// render the stored receipt time through this function.
#[must_use]
pub fn format_preview_currency(receipt_ms: u64) -> String {
    crate::funnel::format_iso_millis(receipt_ms as i64)
}

/// Fixed mode-consequence sentence: a consequence statement, not a sanction
/// prediction (§§3.2, 5.2).
#[must_use]
pub fn preview_mode_sentence(dry_run: bool) -> &'static str {
    if dry_run {
        "A real match would be captured only, not deleted and not violation-counted."
    } else {
        "A real match would enter the delete-first enforcement path subject to \
         target protection and the author's violation history, neither of which was evaluated."
    }
}

/// Fixed caveats, same words every time (§3.2).
pub const PREVIEW_CAVEATS: &str = "Caveats: first matching filter only. Preview recorded nothing; \
    a real post would itself count toward repeat history, unless exempt or stopped by bad_words first. \
    Sanction and protection were not previewed.";

/// Fixed sentence for the T19 `partial` shape (§3.2).
pub const PREVIEW_PARTIAL_SENTENCE: &str =
    "Repeat evaluation is indeterminate; no complete first-match verdict is available.";

/// Generic line for disabled/out-of-scope/DM attempts: no mode, no currency,
/// no verdict fields (§§4.2, 5).
pub const PREVIEW_NOT_EVALUATED_REPLY: &str =
    "Automod preview is not available here. Automod is disabled or this guild is out of scope.";

/// Generic line for rate refusals: no retry-after, no verdict (§4.3).
pub const PREVIEW_RATE_LIMITED_REPLY: &str = "Automod preview rate limit reached. Try again later.";

/// Generic line when the verdict is suppressed (audit failure, timeout,
/// abandoned claim): no verdict is exposed (§6).
pub const PREVIEW_UNAVAILABLE_REPLY: &str =
    "Automod preview is temporarily unavailable. No verdict was recorded.";

/// Render one ephemeral verdict reply. Built only from fixed templates plus
/// the verdict fields and the pre-rendered `currency_iso` timestamp: the
/// reply never echoes the input — no text, normalized text, matched
/// token/host/filename, or input excerpt.
#[must_use]
pub fn render_preview_reply(verdict: &PreviewVerdict, currency_iso: &str) -> String {
    let mode = if verdict.dry_run {
        "dry_run"
    } else {
        "enforce"
    };
    let mut reply = match verdict.result {
        PreviewResult::Matched => {
            let filter = verdict.filter.map_or("none", AutomodFilter::as_str);
            let knob = if verdict.knobs.is_empty() {
                "none (fixed rule)".to_owned()
            } else {
                verdict.knobs.join(", ")
            };
            format!(
                "Automod preview: matched.\nDeciding filter: {filter}.\nDeciding setting: {knob}."
            )
        }
        PreviewResult::NoMatch => {
            "Automod preview: no_match. No filter would decide this text.".to_owned()
        }
        PreviewResult::Exempt => {
            let class = match verdict.exemption {
                Some(PreviewExemption::ExemptChannel) => "exempt_channel",
                Some(PreviewExemption::BypassRole) => "bypass_role",
                None => "none",
            };
            let knob = verdict.knobs.first().copied().unwrap_or("none");
            format!("Automod preview: exempt.\nExemption: {class}.\nDeciding setting: {knob}.")
        }
        PreviewResult::Partial => {
            format!("Automod preview: partial.\n{PREVIEW_PARTIAL_SENTENCE}")
        }
    };
    reply.push_str(&format!(
        "\nMode: {mode}. {}\nConfig current at {currency_iso}.\n{PREVIEW_CAVEATS}",
        preview_mode_sentence(verdict.dry_run)
    ));
    reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automod::{AutomodConfig, AutomodPolicy};

    fn matched_verdict(filter: AutomodFilter, dry_run: bool) -> PreviewVerdict {
        PreviewVerdict {
            result: PreviewResult::Matched,
            filter: Some(filter),
            knobs: crate::automod_preview::deciding_knobs(filter),
            exemption: None,
            dry_run,
        }
    }

    fn exempt_verdict(exemption: PreviewExemption) -> PreviewVerdict {
        PreviewVerdict {
            result: PreviewResult::Exempt,
            filter: None,
            knobs: crate::automod_preview::exemption_knobs(exemption),
            exemption: Some(exemption),
            dry_run: true,
        }
    }

    #[test]
    fn length_buckets_follow_spec_edges() {
        assert_eq!(preview_length_bucket(0), "0_50");
        assert_eq!(preview_length_bucket(50), "0_50");
        assert_eq!(preview_length_bucket(51), "51_500");
        assert_eq!(preview_length_bucket(500), "51_500");
        assert_eq!(preview_length_bucket(501), "501_2000");
        assert_eq!(preview_length_bucket(2000), "501_2000");
    }

    #[test]
    fn audit_reason_is_enums_only() {
        let verdict = matched_verdict(AutomodFilter::BadWords, true);
        assert_eq!(
            audit_reason_for_verdict(&verdict, 12),
            "filter=bad_words;exemption=none;mode=dry_run;len=0_50"
        );
        let verdict = matched_verdict(AutomodFilter::MentionSpam, false);
        assert_eq!(
            audit_reason_for_verdict(&verdict, 600),
            "filter=mention_spam;exemption=none;mode=enforce;len=501_2000"
        );
        // invite_link is a fixed rule: the filter name is recorded, no knob value.
        let verdict = matched_verdict(AutomodFilter::InviteLink, true);
        assert_eq!(
            audit_reason_for_verdict(&verdict, 51),
            "filter=invite_link;exemption=none;mode=dry_run;len=51_500"
        );
        // Exempt rows store filter=none plus the selected class.
        assert_eq!(
            audit_reason_for_verdict(&exempt_verdict(PreviewExemption::ExemptChannel), 7),
            "filter=none;exemption=exempt_channel;mode=dry_run;len=0_50"
        );
        assert_eq!(
            audit_reason_for_verdict(&exempt_verdict(PreviewExemption::BypassRole), 7),
            "filter=none;exemption=bypass_role;mode=dry_run;len=0_50"
        );
        // Partial always means indeterminate: filter=none, exemption=none.
        let partial = PreviewVerdict::partial(true);
        assert_eq!(
            audit_reason_for_verdict(&partial, 9),
            "filter=none;exemption=none;mode=dry_run;len=0_50"
        );
        assert_eq!(
            audit_reason_rate_limited(false, 1200),
            "filter=none;exemption=none;mode=enforce;len=501_2000"
        );
    }

    #[test]
    fn metric_vocabularies_are_closed() {
        for verdict in [
            matched_verdict(AutomodFilter::BadWords, true),
            PreviewVerdict {
                result: PreviewResult::NoMatch,
                filter: None,
                knobs: &[],
                exemption: None,
                dry_run: false,
            },
            exempt_verdict(PreviewExemption::BypassRole),
            PreviewVerdict::partial(true),
        ] {
            let labels = metric_for_verdict(&verdict);
            assert!(
                matches!(labels.result, "matched" | "no_match" | "exempt" | "partial"),
                "unexpected result {}",
                labels.result
            );
            assert!(
                [
                    "bad_words",
                    "repeated_message",
                    "mention_spam",
                    "invite_link",
                    "external_link",
                    "attachment_type",
                    "none"
                ]
                .contains(&labels.filter),
                "unexpected filter {}",
                labels.filter
            );
            assert!(matches!(labels.mode, "dry_run" | "enforce"));
        }
        assert_eq!(
            metric_rate_limited(true),
            PreviewMetricLabels {
                result: "rate_limited",
                filter: "none",
                mode: "dry_run",
            }
        );
        assert_eq!(
            metric_not_evaluated(),
            PreviewMetricLabels {
                result: "not_evaluated",
                filter: "none",
                mode: "not_evaluated",
            }
        );
        assert_eq!(metric_unavailable(None).mode, "not_evaluated");
        assert_eq!(metric_unavailable(Some(true)).mode, "dry_run");
        assert_eq!(metric_unavailable(Some(false)).mode, "enforce");
    }

    #[test]
    fn replay_knob_comes_from_the_stored_class_only() {
        assert_eq!(
            replay_exemption_knob("exempt_channel"),
            Some("TWO_AUTOMOD_EXEMPT_CHANNEL_IDS")
        );
        assert_eq!(
            replay_exemption_knob("bypass_role"),
            Some("TWO_AUTOMOD_BYPASS_ROLE_IDS")
        );
        // Missing/invalid classes fail closed: no guess, no fresh evaluation.
        for bad in [
            "",
            "none",
            "exempt_Channel",
            "admin",
            "TWO_AUTOMOD_BYPASS_ROLE_IDS",
        ] {
            assert_eq!(replay_exemption_knob(bad), None, "class {bad:?}");
        }
    }

    #[test]
    fn replies_carry_verdict_fields_but_never_input() {
        let currency = format_preview_currency(1_700_000_000_000);
        let adversarial = "bannedword <@everyone> https://evil.example/x **bold**";
        for verdict in [
            matched_verdict(AutomodFilter::BadWords, true),
            matched_verdict(AutomodFilter::InviteLink, false),
            PreviewVerdict {
                result: PreviewResult::NoMatch,
                filter: None,
                knobs: &[],
                exemption: None,
                dry_run: true,
            },
            exempt_verdict(PreviewExemption::ExemptChannel),
            PreviewVerdict::partial(false),
        ] {
            let reply = render_preview_reply(&verdict, &currency);
            for token in adversarial.split(' ') {
                assert!(
                    !reply.contains(token),
                    "reply echoes input token {token:?}: {reply}"
                );
            }
            assert!(reply.contains(&currency), "missing currency: {reply}");
            assert!(reply.contains(PREVIEW_CAVEATS), "missing caveats: {reply}");
        }
        // Mode sentences differ between dry-run and enforce for the same filter.
        let dry = render_preview_reply(&matched_verdict(AutomodFilter::BadWords, true), &currency);
        let enforced =
            render_preview_reply(&matched_verdict(AutomodFilter::BadWords, false), &currency);
        assert_ne!(dry, enforced);
        assert!(dry.contains("not deleted and not violation-counted"));
        assert!(enforced.contains("delete-first enforcement path"));
        // Partial carries the fixed sentence and no deciding filter or knob.
        let partial = render_preview_reply(&PreviewVerdict::partial(true), &currency);
        assert!(partial.contains(PREVIEW_PARTIAL_SENTENCE));
        assert!(!partial.contains("Deciding filter"));
        assert!(!partial.contains("Deciding setting"));
        // Generic lines carry no verdict fields, mode, or currency.
        for generic in [
            PREVIEW_NOT_EVALUATED_REPLY,
            PREVIEW_RATE_LIMITED_REPLY,
            PREVIEW_UNAVAILABLE_REPLY,
        ] {
            assert!(!generic.contains("Deciding"), "generic leaks: {generic}");
            assert!(!generic.contains("dry_run") && !generic.contains("enforce"));
            assert!(!generic.contains("current at"));
        }
    }

    #[test]
    fn same_verdict_same_reply_for_different_inputs() {
        // The renderer takes no content parameter, so two different texts with
        // the same verdict render byte-identical replies.
        let currency = format_preview_currency(1_700_000_000_123);
        let policy = AutomodPolicy {
            bad_words: vec!["bannedword".to_owned()],
            ..AutomodPolicy::default()
        };
        let config = AutomodConfig {
            enabled: true,
            dry_run: true,
            policy,
        };
        let repeats = crate::automod::RepeatTracker::default();
        let mk = |content: &str| crate::automod::AutomodMessage {
            guild_id: "g".to_owned(),
            channel_id: "c".to_owned(),
            message_id: "preview:i".to_owned(),
            author_id: "a".to_owned(),
            author_is_bot: false,
            role_ids: vec![],
            content: content.to_owned(),
            mentioned_user_ids: vec![],
            attachment_names: vec![],
            observed_timestamp_ms: 1,
        };
        let a = crate::automod_preview::preview_automod(
            &mk("has bannedword one"),
            0,
            &config,
            &repeats,
        );
        let b = crate::automod_preview::preview_automod(
            &mk("has bannedword two"),
            0,
            &config,
            &repeats,
        );
        assert_eq!(a, b);
        assert_eq!(
            render_preview_reply(&a, &currency),
            render_preview_reply(&b, &currency)
        );
    }
}
