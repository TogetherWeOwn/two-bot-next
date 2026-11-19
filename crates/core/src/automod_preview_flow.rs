//! Pure decisions for the `/automod-preview` claim protocol.
//!
//! The caller owns authorization and I/O. Decode and scope refusals stop
//! before the caller takes a transaction claim or charges the limiter.

use crate::automod_preview_audit::preview_audit_id;
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

#[cfg(test)]
mod tests {
    use super::*;

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
