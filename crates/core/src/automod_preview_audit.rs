//! Content-free redelivery replay for `/automod-preview` (spec §6).
//!
//! Rebuild replies only from the committed outcome, reason, and timestamp.
//! Malformed stored data fails closed instead of triggering re-evaluation.

use crate::automod::AutomodFilter;
use crate::automod_preview::{PreviewExemption, PreviewResult, PreviewVerdict};
use crate::automod_preview_report::{
    render_preview_reply, replay_exemption_knob, PREVIEW_RATE_LIMITED_REPLY,
};

/// Parsed `reason` field (`filter=..;exemption=..;mode=..;len=..`). Only the
/// filter, exemption, and mode drive a replayed reply; the length bucket is
/// validated present but never rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedPreviewReason {
    filter: String,
    exemption: String,
    dry_run: bool,
}

/// Strict parse of a stored preview reason. `None` fails closed as
/// unavailable: never a guessed class or a fresh evaluation.
fn parse_preview_reason(reason: &str) -> Option<ParsedPreviewReason> {
    let mut parts = reason.split(';');
    let filter = parts.next()?.strip_prefix("filter=")?;
    let exemption = parts.next()?.strip_prefix("exemption=")?;
    let mode = parts.next()?.strip_prefix("mode=")?;
    let len = parts.next()?.strip_prefix("len=")?;
    if parts.next().is_some() || !matches!(len, "0_50" | "51_500" | "501_2000") {
        return None;
    }
    let dry_run = match mode {
        "dry_run" => true,
        "enforce" => false,
        _ => return None,
    };
    Some(ParsedPreviewReason {
        filter: filter.to_owned(),
        exemption: exemption.to_owned(),
        dry_run,
    })
}

/// Rebuild the ephemeral reply from one committed audit row, without
/// evaluating anything. Returns `None` for any missing, unknown, or
/// malformed field: the handler answers the generic unavailable line and
/// never guesses a verdict.
///
/// The exemption knob comes from the persisted class via
/// [`replay_exemption_knob`], never from current config or member roles; a
/// `matched` filter name parses via [`AutomodFilter::from_persisted_name`].
/// `rate_limited` replays the generic rate line (the mode still parses, so a
/// corrupt reason fails closed). `created_at` is the stored timestamp text,
/// used verbatim so replays match the first reply exactly.
#[must_use]
pub fn replay_preview_reply(
    outcome: &str,
    reason: Option<&str>,
    created_at: &str,
) -> Option<String> {
    if created_at.is_empty() {
        return None;
    }
    if outcome == "rate_limited" {
        let parsed = parse_preview_reason(reason?)?;
        if parsed.filter != "none" || parsed.exemption != "none" {
            return None;
        }
        return Some(PREVIEW_RATE_LIMITED_REPLY.to_owned());
    }
    let parsed = parse_preview_reason(reason?)?;
    let verdict = match outcome {
        "matched" => {
            if parsed.exemption != "none" {
                return None;
            }
            let filter = AutomodFilter::from_persisted_name(&parsed.filter)?;
            PreviewVerdict {
                result: PreviewResult::Matched,
                filter: Some(filter),
                knobs: crate::automod_preview::deciding_knobs(filter),
                exemption: None,
                dry_run: parsed.dry_run,
            }
        }
        "no_match" => {
            if parsed.filter != "none" || parsed.exemption != "none" {
                return None;
            }
            PreviewVerdict {
                result: PreviewResult::NoMatch,
                filter: None,
                knobs: &[],
                exemption: None,
                dry_run: parsed.dry_run,
            }
        }
        "exempt" => {
            if parsed.filter != "none" {
                return None;
            }
            let class = match parsed.exemption.as_str() {
                "exempt_channel" => PreviewExemption::ExemptChannel,
                "bypass_role" => PreviewExemption::BypassRole,
                _ => return None,
            };
            // The knob comes from the stored class, never current config.
            replay_exemption_knob(parsed.exemption.as_str())?;
            PreviewVerdict {
                result: PreviewResult::Exempt,
                filter: None,
                knobs: crate::automod_preview::exemption_knobs(class),
                exemption: Some(class),
                dry_run: parsed.dry_run,
            }
        }
        "partial" => {
            if parsed.filter != "none" || parsed.exemption != "none" {
                return None;
            }
            PreviewVerdict::partial(parsed.dry_run)
        }
        _ => return None,
    };
    Some(render_preview_reply(&verdict, created_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automod::{AutomodConfig, AutomodMessage, AutomodPolicy, RepeatTracker};
    use crate::automod_preview::preview_automod;
    use crate::automod_preview_report::{audit_reason_for_verdict, preview_audit_outcome};

    #[test]
    fn reason_parse_accepts_only_the_emitted_shape() {
        let parsed = parse_preview_reason("filter=bad_words;exemption=none;mode=dry_run;len=0_50")
            .expect("emitted shape parses");
        assert_eq!(parsed.filter, "bad_words");
        assert_eq!(parsed.exemption, "none");
        assert!(parsed.dry_run);
        let enforced =
            parse_preview_reason("filter=none;exemption=bypass_role;mode=enforce;len=51_500")
                .expect("enforce parses");
        assert!(!enforced.dry_run);
        for bad in [
            "",
            "filter=bad_words",
            "filter=bad_words;exemption=none;mode=dry_run",
            "filter=bad_words;exemption=none;mode=dry_run;len=0_50;extra=x",
            "filter=bad_words;exemption=none;mode=live;len=0_50",
            "filter=bad_words;exemption=none;mode=dry_run;len=",
            "filter=bad_words;exemption=none;mode=dry_run;len=unexpected",
            "exemption=none;filter=bad_words;mode=dry_run;len=0_50",
            "filter:bad_words;exemption=none;mode=dry_run;len=0_50",
        ] {
            assert_eq!(parse_preview_reason(bad), None, "reason {bad:?}");
        }
    }

    fn matched_bad_words_verdict(dry_run: bool) -> PreviewVerdict {
        PreviewVerdict {
            result: PreviewResult::Matched,
            filter: Some(AutomodFilter::BadWords),
            knobs: crate::automod_preview::deciding_knobs(AutomodFilter::BadWords),
            exemption: None,
            dry_run,
        }
    }

    #[test]
    fn replay_rebuilds_every_outcome_without_evaluating() {
        let created_at = "2026-11-02 08:00:00+00";
        // Matched: first reply and replayed reply are byte-identical.
        let verdict = matched_bad_words_verdict(true);
        let first = render_preview_reply(&verdict, created_at);
        let reason = audit_reason_for_verdict(&verdict, 12);
        assert_eq!(
            replay_preview_reply(preview_audit_outcome(&verdict), Some(&reason), created_at),
            Some(first)
        );
        // No-match, exempt (knob from the stored class), partial, rate-limited.
        let no_match = PreviewVerdict {
            result: PreviewResult::NoMatch,
            filter: None,
            knobs: &[],
            exemption: None,
            dry_run: false,
        };
        let reason = audit_reason_for_verdict(&no_match, 9);
        assert_eq!(
            replay_preview_reply("no_match", Some(&reason), created_at),
            Some(render_preview_reply(&no_match, created_at))
        );
        let exempt = PreviewVerdict {
            result: PreviewResult::Exempt,
            filter: None,
            knobs: crate::automod_preview::exemption_knobs(PreviewExemption::BypassRole),
            exemption: Some(PreviewExemption::BypassRole),
            dry_run: true,
        };
        let reason = audit_reason_for_verdict(&exempt, 7);
        let replayed =
            replay_preview_reply("exempt", Some(&reason), created_at).expect("exempt replays");
        assert_eq!(replayed, render_preview_reply(&exempt, created_at));
        assert!(replayed.contains("TWO_AUTOMOD_BYPASS_ROLE_IDS"));
        let partial = PreviewVerdict::partial(true);
        let reason = audit_reason_for_verdict(&partial, 9);
        assert_eq!(
            replay_preview_reply("partial", Some(&reason), created_at),
            Some(render_preview_reply(&partial, created_at))
        );
        assert_eq!(
            replay_preview_reply(
                "rate_limited",
                Some("filter=none;exemption=none;mode=enforce;len=51_500"),
                created_at
            ),
            Some(PREVIEW_RATE_LIMITED_REPLY.to_owned())
        );
    }

    #[test]
    fn replay_fails_closed_on_any_malformed_field() {
        let created_at = "2026-11-02 08:00:00+00";
        // Unknown outcome, missing/invalid reason, unknown filter, guessed class.
        assert_eq!(
            replay_preview_reply(
                "bogus",
                Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        assert_eq!(replay_preview_reply("matched", None, created_at), None);
        assert_eq!(
            replay_preview_reply(
                "matched",
                Some("filter=nonsense;exemption=none;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        // Matched row claiming an exemption, exempt row with a filter, and an
        // invalid stored exemption class all refuse instead of guessing.
        assert_eq!(
            replay_preview_reply(
                "matched",
                Some("filter=bad_words;exemption=bypass_role;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        assert_eq!(
            replay_preview_reply(
                "no_match",
                Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        assert_eq!(
            replay_preview_reply(
                "exempt",
                Some("filter=none;exemption=admin;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        assert_eq!(
            replay_preview_reply(
                "exempt",
                Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        for (outcome, reason) in [
            (
                "exempt",
                "filter=bad_words;exemption=bypass_role;mode=dry_run;len=0_50",
            ),
            (
                "partial",
                "filter=bad_words;exemption=none;mode=dry_run;len=0_50",
            ),
            (
                "partial",
                "filter=none;exemption=bypass_role;mode=dry_run;len=0_50",
            ),
            (
                "rate_limited",
                "filter=bad_words;exemption=none;mode=dry_run;len=0_50",
            ),
            (
                "rate_limited",
                "filter=none;exemption=bypass_role;mode=dry_run;len=0_50",
            ),
        ] {
            assert_eq!(
                replay_preview_reply(outcome, Some(reason), created_at),
                None
            );
        }
        // Corrupt rate-limited reason and empty timestamp refuse too.
        assert_eq!(
            replay_preview_reply(
                "rate_limited",
                Some("filter=none;exemption=none;mode=live;len=0_50"),
                created_at
            ),
            None
        );
        assert_eq!(
            replay_preview_reply(
                "no_match",
                Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
                ""
            ),
            None
        );
    }

    #[test]
    fn replay_never_consults_live_config_or_roles() {
        // The stored row was written under a config that has since changed:
        // replay still renders the stored filter and knob, byte-identical.
        let created_at = "2026-11-02 08:00:00+00";
        let verdict = matched_bad_words_verdict(true);
        let reason = audit_reason_for_verdict(&verdict, 12);
        let before = replay_preview_reply("matched", Some(&reason), created_at).expect("replays");
        // A "changed world": different policy, different roles — replay takes
        // neither as a parameter, so the stored row renders the same reply.
        let policy = AutomodPolicy {
            bad_words: vec!["different".to_owned()],
            ..AutomodPolicy::default()
        };
        let _config = AutomodConfig {
            enabled: true,
            dry_run: false,
            policy,
        };
        let after = replay_preview_reply("matched", Some(&reason), created_at).expect("replays");
        assert_eq!(before, after);
        assert!(after.contains("TWO_AUTOMOD_BAD_WORDS"));
    }

    #[test]
    fn replay_matches_a_live_evaluation_for_the_same_verdict() {
        // Core-half of T17: the row written for a real evaluation replays the
        // same reply the evaluation rendered.
        let policy = AutomodPolicy {
            bad_words: vec!["bannedword".to_owned()],
            ..AutomodPolicy::default()
        };
        let config = AutomodConfig {
            enabled: true,
            dry_run: true,
            policy,
        };
        let message = AutomodMessage {
            guild_id: "g".to_owned(),
            channel_id: "c".to_owned(),
            message_id: "preview:live".to_owned(),
            author_id: "a".to_owned(),
            author_is_bot: false,
            role_ids: vec![],
            content: "has bannedword here".to_owned(),
            mentioned_user_ids: vec![],
            attachment_names: vec![],
            observed_timestamp_ms: 1,
        };
        let verdict = preview_automod(&message, 0, &config, &RepeatTracker::default());
        assert_eq!(verdict.result, PreviewResult::Matched);
        let created_at = "2026-11-02 08:00:00+00";
        let first = render_preview_reply(&verdict, created_at);
        let reason = audit_reason_for_verdict(&verdict, 17);
        assert_eq!(
            replay_preview_reply(preview_audit_outcome(&verdict), Some(&reason), created_at),
            Some(first)
        );
    }
}
