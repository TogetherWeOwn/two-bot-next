//! Fail-closed reconstruction of rate-limited automod preview replies.
//!
//! Other persisted outcomes remain unavailable until their replay support and
//! safety tests are restored. This helper must not be wired to a handler yet.

use crate::automod_preview_report::PREVIEW_RATE_LIMITED_REPLY;

#[derive(Debug, PartialEq, Eq)]
struct ParsedPreviewReason<'a> {
    filter: &'a str,
    exemption: &'a str,
}

fn parse_preview_reason(reason: &str) -> Option<ParsedPreviewReason<'_>> {
    let mut parts = reason.split(';');
    let filter = parts.next()?.strip_prefix("filter=")?;
    let exemption = parts.next()?.strip_prefix("exemption=")?;
    let mode = parts.next()?.strip_prefix("mode=")?;
    let len = parts.next()?.strip_prefix("len=")?;
    if parts.next().is_some()
        || !matches!(mode, "dry_run" | "enforce")
        || !matches!(len, "0_50" | "51_500" | "501_2000")
    {
        return None;
    }
    Some(ParsedPreviewReason { filter, exemption })
}

/// Reconstruct a rate-limited reply from a committed row without evaluation.
/// All other outcomes and invalid fields fail closed as unavailable.
#[must_use]
pub fn replay_preview_reply(
    outcome: &str,
    reason: Option<&str>,
    created_at: &str,
) -> Option<String> {
    if created_at.is_empty() || outcome != "rate_limited" {
        return None;
    }
    let parsed = parse_preview_reason(reason?)?;
    if parsed.filter != "none" || parsed.exemption != "none" {
        return None;
    }
    Some(PREVIEW_RATE_LIMITED_REPLY.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn unsupported_outcomes_fail_closed() {
        let reason = Some("filter=none;exemption=none;mode=dry_run;len=0_50");
        for outcome in ["matched", "no_match", "exempt", "partial", "bogus", ""] {
            assert_eq!(replay_preview_reply(outcome, reason, "t"), None);
        }
    }
}
