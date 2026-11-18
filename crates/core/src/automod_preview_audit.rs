//! Fail-closed replay of supported automod preview rows.
//!
//! This helper is not wired to the preview handler.

use crate::automod::AutomodFilter;
use crate::automod_preview::{
    deciding_knobs, exemption_knobs, PreviewExemption, PreviewResult, PreviewVerdict,
};
use crate::automod_preview_report::{render_preview_reply, PREVIEW_RATE_LIMITED_REPLY};

#[derive(Debug, PartialEq, Eq)]
struct ParsedPreviewReason<'a> {
    filter: &'a str,
    exemption: &'a str,
    dry_run: bool,
}

fn parse_preview_reason(reason: &str) -> Option<ParsedPreviewReason<'_>> {
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
        filter,
        exemption,
        dry_run,
    })
}

/// Reconstruct a supported reply from a committed row without evaluation.
/// `created_at` is the pre-rendered stored currency timestamp, not a new clock
/// reading. Unsupported outcomes, invalid reasons, and empty timestamps fail closed.
#[must_use]
pub fn replay_preview_reply(
    outcome: &str,
    reason: Option<&str>,
    created_at: &str,
) -> Option<String> {
    if created_at.is_empty() {
        return None;
    }
    let parsed = parse_preview_reason(reason?)?;
    let verdict = match (outcome, parsed.filter, parsed.exemption) {
        ("matched", name, "none") => {
            let filter = AutomodFilter::from_persisted_name(name)?;
            PreviewVerdict {
                result: PreviewResult::Matched,
                filter: Some(filter),
                knobs: deciding_knobs(filter),
                exemption: None,
                dry_run: parsed.dry_run,
            }
        }
        ("rate_limited", "none", "none") => return Some(PREVIEW_RATE_LIMITED_REPLY.to_owned()),
        ("no_match", "none", "none") => PreviewVerdict {
            result: PreviewResult::NoMatch,
            filter: None,
            knobs: &[],
            exemption: None,
            dry_run: parsed.dry_run,
        },
        ("partial", "none", "none") => PreviewVerdict::partial(parsed.dry_run),
        ("exempt", "none", class) => {
            let exemption = match class {
                "exempt_channel" => PreviewExemption::ExemptChannel,
                "bypass_role" => PreviewExemption::BypassRole,
                _ => return None,
            };
            PreviewVerdict {
                result: PreviewResult::Exempt,
                filter: None,
                knobs: exemption_knobs(exemption),
                exemption: Some(exemption),
                dry_run: parsed.dry_run,
            }
        }
        _ => return None,
    };
    Some(render_preview_reply(&verdict, created_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automod_preview_report::{audit_reason_for_verdict, preview_audit_outcome};

    #[test]
    fn rate_limit_replays_each_emitted_mode_and_bucket() {
        for mode in ["dry_run", "enforce"] {
            for len in ["0_50", "51_500", "501_2000"] {
                let reason = format!("filter=none;exemption=none;mode={mode};len={len}");
                assert_eq!(
                    replay_preview_reply("rate_limited", Some(&reason), "stored timestamp"),
                    Some(PREVIEW_RATE_LIMITED_REPLY.to_owned())
                );
            }
        }
    }

    #[test]
    fn malformed_or_contradictory_rate_limit_rows_fail_closed() {
        for reason in [
            "",
            "filter=none",
            "filter=none;exemption=none;mode=dry_run",
            "filter=none;exemption=none;mode=dry_run;len=0_50;extra=x",
            "filter=none;exemption=none;mode=live;len=0_50",
            "filter=none;exemption=none;mode=dry_run;len=",
            "filter=none;exemption=none;mode=dry_run;len=unexpected",
            "exemption=none;filter=none;mode=dry_run;len=0_50",
            "filter:none;exemption=none;mode=dry_run;len=0_50",
            "filter=bad_words;exemption=none;mode=dry_run;len=0_50",
            "filter=none;exemption=bypass_role;mode=dry_run;len=0_50",
        ] {
            assert_eq!(
                replay_preview_reply("rate_limited", Some(reason), "t"),
                None
            );
        }
        assert_eq!(replay_preview_reply("rate_limited", None, "t"), None);
        assert_eq!(
            replay_preview_reply(
                "rate_limited",
                Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
                ""
            ),
            None
        );
    }

    #[test]
    fn no_match_and_partial_replay_the_original_reply() {
        let created_at = "2026-11-02T08:00:00.123Z";
        for dry_run in [true, false] {
            let no_match = PreviewVerdict {
                result: PreviewResult::NoMatch,
                filter: None,
                knobs: &[],
                exemption: None,
                dry_run,
            };
            for verdict in [no_match, PreviewVerdict::partial(dry_run)] {
                let outcome = preview_audit_outcome(&verdict);
                for len in [50, 51, 501] {
                    let reason = audit_reason_for_verdict(&verdict, len);
                    assert_eq!(
                        replay_preview_reply(outcome, Some(&reason), created_at),
                        Some(render_preview_reply(&verdict, created_at)),
                        "{outcome}: {reason}"
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_or_contradictory_verdict_rows_fail_closed() {
        let valid = Some("filter=none;exemption=none;mode=dry_run;len=0_50");
        for outcome in ["no_match", "partial"] {
            for reason in [
                None,
                Some("filter=none;exemption=none;mode=dry_run"),
                Some("filter=none;exemption=none;mode=live;len=0_50"),
                Some("filter=none;exemption=none;mode=dry_run;len=unexpected"),
                Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50"),
                Some("filter=none;exemption=bypass_role;mode=dry_run;len=0_50"),
            ] {
                assert_eq!(replay_preview_reply(outcome, reason, "t"), None);
            }
            assert_eq!(replay_preview_reply(outcome, valid, ""), None);
        }
    }

    #[test]
    fn unknown_outcomes_and_missing_exemption_fail_closed() {
        let reason = Some("filter=none;exemption=none;mode=dry_run;len=0_50");
        for outcome in ["exempt", "bogus", ""] {
            assert_eq!(replay_preview_reply(outcome, reason, "t"), None);
        }
        assert_eq!(
            replay_preview_reply(
                "bogus",
                Some("filter=none;exemption=bypass_role;mode=dry_run;len=0_50"),
                "t",
            ),
            None
        );
    }

    #[test]
    fn matched_replays_the_original_reply_for_each_filter_and_mode() {
        let created_at = "2026-11-02T08:00:00.123Z";
        for filter in [
            AutomodFilter::BadWords,
            AutomodFilter::RepeatedMessage,
            AutomodFilter::MentionSpam,
            AutomodFilter::InviteLink,
            AutomodFilter::ExternalLink,
            AutomodFilter::AttachmentType,
        ] {
            for dry_run in [true, false] {
                let verdict = PreviewVerdict {
                    result: PreviewResult::Matched,
                    filter: Some(filter),
                    knobs: deciding_knobs(filter),
                    exemption: None,
                    dry_run,
                };
                let outcome = preview_audit_outcome(&verdict);
                for len in [50, 51, 501] {
                    let reason = audit_reason_for_verdict(&verdict, len);
                    assert_eq!(
                        replay_preview_reply(outcome, Some(&reason), created_at),
                        Some(render_preview_reply(&verdict, created_at)),
                        "{outcome}: {reason}"
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_or_contradictory_matched_rows_fail_closed() {
        let valid = Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50");
        for reason in [
            None,
            Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
            Some("filter=unknown;exemption=none;mode=dry_run;len=0_50"),
            Some("filter=bad_words;exemption=bypass_role;mode=dry_run;len=0_50"),
            Some("filter=bad_words;exemption=exempt_channel;mode=enforce;len=0_50"),
            Some("filter=bad_words;exemption=none;mode=live;len=0_50"),
            Some("filter=bad_words;exemption=none;mode=dry_run;len=unexpected"),
            Some("filter=bad_words;exemption=none;mode=dry_run"),
            Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50;filter=invite_link"),
        ] {
            assert_eq!(
                replay_preview_reply("matched", reason, "t"),
                None,
                "{reason:?}"
            );
        }
        assert_eq!(replay_preview_reply("matched", valid, ""), None);
    }

    #[test]
    fn exempt_replays_the_original_reply_for_each_class_and_mode() {
        let created_at = "2026-11-02T08:00:00.123Z";
        for exemption in [
            PreviewExemption::ExemptChannel,
            PreviewExemption::BypassRole,
        ] {
            for dry_run in [true, false] {
                let verdict = PreviewVerdict {
                    result: PreviewResult::Exempt,
                    filter: None,
                    knobs: exemption_knobs(exemption),
                    exemption: Some(exemption),
                    dry_run,
                };
                let outcome = preview_audit_outcome(&verdict);
                for len in [50, 51, 501] {
                    let reason = audit_reason_for_verdict(&verdict, len);
                    assert_eq!(
                        replay_preview_reply(outcome, Some(&reason), created_at),
                        Some(render_preview_reply(&verdict, created_at)),
                        "{outcome}: {reason}"
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_or_contradictory_exempt_rows_fail_closed() {
        let valid = Some("filter=none;exemption=bypass_role;mode=dry_run;len=0_50");
        for reason in [
            None,
            Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
            Some("filter=none;exemption=admin;mode=dry_run;len=0_50"),
            Some("filter=bad_words;exemption=bypass_role;mode=dry_run;len=0_50"),
            Some("filter=none;exemption=exempt_channel;mode=live;len=0_50"),
            Some("filter=none;exemption=bypass_role;mode=dry_run;len=unexpected"),
            Some("filter=none;exemption=exempt_channel;mode=enforce"),
        ] {
            assert_eq!(
                replay_preview_reply("exempt", reason, "t"),
                None,
                "{reason:?}"
            );
        }
        assert_eq!(replay_preview_reply("exempt", valid, ""), None);
    }
}
