//! Pure decisions for the `/automod-preview` claim protocol.
//!
//! The caller owns authorization and I/O. Decode and scope refusals stop
//! before the caller takes a transaction claim or charges the limiter.

use crate::automod_preview_audit::{preview_audit_id, replay_preview_reply, PreviewAuditRow};
use crate::automod_preview_input::{DecodedPreviewInput, PreviewInputRefusal};
use crate::automod_preview_report::{
    metric_not_evaluated, PreviewMetricLabels, PREVIEW_NOT_EVALUATED_REPLY,
};

/// Answer a refusal immediately or proceed to claim the interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreClaimPlan {
    /// Decode refusals emit no metric; scope refusals emit `not_evaluated`.
    AnswerNow {
        reply: String,
        metric: Option<PreviewMetricLabels>,
    },
    /// Take the transaction-scoped claim and read the audit row on that connection.
    Claim {
        audit_id: String,
        text_len_utf16: u32,
    },
}

/// Give the strict-decode result precedence over the boot and guild scope gates.
/// The router must already have checked permissions. A DM is out of scope.
#[must_use]
pub fn plan_pre_claim(
    decoded: Result<&DecodedPreviewInput, PreviewInputRefusal>,
    automod_enabled: bool,
    in_scope_guild: bool,
    interaction_id: &str,
) -> PreClaimPlan {
    let input = match decoded {
        Err(refusal) => {
            return PreClaimPlan::AnswerNow {
                reply: refusal.message().to_owned(),
                metric: None,
            };
        }
        Ok(input) => input,
    };
    if !automod_enabled || !in_scope_guild {
        return PreClaimPlan::AnswerNow {
            reply: PREVIEW_NOT_EVALUATED_REPLY.to_owned(),
            metric: Some(metric_not_evaluated()),
        };
    }
    PreClaimPlan::Claim {
        audit_id: preview_audit_id(interaction_id),
        text_len_utf16: input.text_len_utf16,
    }
}

/// Decide whether to replay the stored result or proceed to the limiter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostClaimPlan {
    /// A stored row ends the attempt without charging or evaluating.
    Replay {
        /// `None` means the row is malformed; answer unavailable, not fresh.
        reply: Option<String>,
    },
    /// No row was found. A first attempt or a retry after rollback may proceed.
    Fresh,
}

/// Plan from a successful audit read under the transaction claim.
/// A read error is not a missing row; handle it before calling this function.
#[must_use]
pub fn plan_post_claim(row: Option<&PreviewAuditRow>) -> PostClaimPlan {
    match row {
        Some(row) => PostClaimPlan::Replay {
            reply: replay_preview_reply(&row.outcome, row.reason.as_deref(), &row.created_at),
        },
        None => PostClaimPlan::Fresh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automod::AutomodFilter;
    use crate::automod_preview::{
        deciding_knobs, exemption_knobs, PreviewExemption, PreviewResult, PreviewVerdict,
    };
    use crate::automod_preview_report::{
        audit_reason_for_verdict, audit_reason_rate_limited, preview_audit_outcome,
        render_preview_reply, PREVIEW_RATE_LIMITED_REPLY,
    };

    #[test]
    fn stored_row_replays_the_original_reply() {
        let created_at = "2026-11-02T08:00:00.123456Z";
        let filter = AutomodFilter::BadWords;
        let exemption = PreviewExemption::BypassRole;
        let verdicts = [
            PreviewVerdict {
                result: PreviewResult::Matched,
                filter: Some(filter),
                knobs: deciding_knobs(filter),
                exemption: None,
                dry_run: true,
            },
            PreviewVerdict {
                result: PreviewResult::NoMatch,
                filter: None,
                knobs: &[],
                exemption: None,
                dry_run: false,
            },
            PreviewVerdict {
                result: PreviewResult::Exempt,
                filter: None,
                knobs: exemption_knobs(exemption),
                exemption: Some(exemption),
                dry_run: true,
            },
            PreviewVerdict::partial(true),
        ];
        for verdict in &verdicts {
            let row = PreviewAuditRow {
                outcome: preview_audit_outcome(verdict).to_owned(),
                reason: Some(audit_reason_for_verdict(verdict, 12)),
                created_at: created_at.to_owned(),
            };
            assert_eq!(
                plan_post_claim(Some(&row)),
                PostClaimPlan::Replay {
                    reply: Some(render_preview_reply(verdict, created_at)),
                }
            );
        }
        let rate_limited = PreviewAuditRow {
            outcome: "rate_limited".to_owned(),
            reason: Some(audit_reason_rate_limited(false, 600)),
            created_at: created_at.to_owned(),
        };
        assert_eq!(
            plan_post_claim(Some(&rate_limited)),
            PostClaimPlan::Replay {
                reply: Some(PREVIEW_RATE_LIMITED_REPLY.to_owned()),
            }
        );
    }

    #[test]
    fn only_an_absent_row_allows_a_fresh_attempt() {
        let created_at = "2026-11-02T08:00:00.123456Z";
        let reason = "filter=none;exemption=none;mode=dry_run;len=0_50";
        for (outcome, reason, timestamp) in [
            ("bogus", Some(reason), created_at),
            (
                "matched",
                Some("filter=nonsense;exemption=none;mode=dry_run;len=0_50"),
                created_at,
            ),
            ("no_match", None, created_at),
            ("no_match", Some(reason), ""),
        ] {
            let row = PreviewAuditRow {
                outcome: outcome.to_owned(),
                reason: reason.map(str::to_owned),
                created_at: timestamp.to_owned(),
            };
            assert_eq!(
                plan_post_claim(Some(&row)),
                PostClaimPlan::Replay { reply: None }
            );
        }
        assert_eq!(plan_post_claim(None), PostClaimPlan::Fresh);
    }

    #[test]
    fn decode_refusal_answers_now_with_no_metric() {
        use PreviewInputRefusal::*;
        for refusal in [
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
        ] {
            for enabled in [false, true] {
                for in_scope in [false, true] {
                    assert_eq!(
                        plan_pre_claim(Err(refusal), enabled, in_scope, "7"),
                        PreClaimPlan::AnswerNow {
                            reply: refusal.message().to_owned(),
                            metric: None,
                        },
                        "refusal={refusal:?} enabled={enabled} in_scope={in_scope}"
                    );
                }
            }
        }
    }

    #[test]
    fn gate_matrix_refuses_before_any_claim() {
        let input = DecodedPreviewInput {
            text: "synthetic".to_owned(),
            text_len_utf16: 9,
            mention_count: 0,
            attachment_names: vec![],
        };
        for (enabled, in_scope) in [(false, false), (false, true), (true, false)] {
            assert_eq!(
                plan_pre_claim(Ok(&input), enabled, in_scope, "7"),
                PreClaimPlan::AnswerNow {
                    reply: PREVIEW_NOT_EVALUATED_REPLY.to_owned(),
                    metric: Some(metric_not_evaluated()),
                },
                "enabled={enabled} in_scope={in_scope}"
            );
        }
        assert_eq!(
            plan_pre_claim(Ok(&input), true, true, "interaction-7"),
            PreClaimPlan::Claim {
                audit_id: "automod-preview:interaction-7".to_owned(),
                text_len_utf16: 9,
            }
        );
    }
}
