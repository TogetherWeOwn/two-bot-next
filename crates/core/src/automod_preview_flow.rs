//! Handler orchestration order for `/automod-preview`
//! (docs/automod-preview-spec.md §§4.3, 6; slice 3f).
//!
//! Pure core: no I/O, no clock, no config read. The Discord handler owns the
//! live-snapshot refresh, the runtime lock, and the transaction; it calls into
//! this module to decide *what to do next* so the §6 protocol lives in code,
//! not prose:
//!
//! 1. [`plan_pre_claim`]: strict-decode refusal or scope gate first. Either
//!    answers immediately — no lock, no limiter charge, no audit row. Decode
//!    refusals carry no oracle bit, so they emit no metric either; gate
//!    refusals emit `not_evaluated` with no row.
//! 2. The winner takes the transaction-scoped advisory lock and reads the
//!    audit id on that same connection (slice 3e), then calls
//!    [`plan_post_claim`]: a committed row replays with no charge and no
//!    evaluation; no row goes fresh. An unknown commit outcome must only take
//!    this same locked replay read — never an assumed rollback.
//! 3. On [`PostClaimPlan::Fresh`] the handler runs the limiter, then
//!    [`plan_fresh`]: `ReplayDuplicate` is an idempotent retry of a rolled-back
//!    attempt, so it evaluates without double-charging; only `RateLimited`
//!    refuses, evaluating nothing.
//! 4. [`evaluated_commit`] / [`rate_limited_commit`] bundle the audit
//!    outcome, reason, record, and metric for the single INSERT the winner
//!    commits before any verdict reply. Audit failure suppresses the verdict
//!    (generic unavailable); the metric uses `unavailable` with the effective
//!    mode when a snapshot exists.
//!
//! The runtime mutex is released before any audit I/O await; it is never held
//! across the transaction's awaits. Nothing here takes the previewed text, so
//! the reply and the row cannot carry content by construction.

use crate::automod_preview::PreviewVerdict;
use crate::automod_preview_audit::{preview_audit_id, replay_preview_reply, PreviewAuditRow};
use crate::automod_preview_input::{DecodedPreviewInput, PreviewInputRefusal};
use crate::automod_preview_limiter::PreviewRateDecision;
use crate::automod_preview_report::PreviewMetricLabels;
use crate::automod_preview_report::{
    audit_reason_for_verdict, audit_reason_rate_limited, metric_for_verdict, metric_not_evaluated,
    metric_rate_limited, preview_audit_outcome, PREVIEW_NOT_EVALUATED_REPLY,
};
use crate::custom_commands::AuditRecord;

/// Pre-claim decision: answer now, or take the transaction lock and read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreClaimPlan {
    /// Answer immediately: no lock, no limiter charge, no audit row.
    /// `metric` is `None` for strict-decode refusals (no oracle bit, so no
    /// metric) and `Some(not_evaluated)` for scope-gate refusals.
    AnswerNow {
        reply: String,
        metric: Option<PreviewMetricLabels>,
    },
    /// Serialize by interaction id, then call [`plan_post_claim`] with the
    /// locked replay read before deciding to replay or evaluate.
    Claim {
        audit_id: String,
        text_len_utf16: u32,
    },
}

/// Strict decode and scope gate, before any budget or database touch.
///
/// `decoded` is the strict-decode result; `automod_enabled` is the boot gate
/// and `in_scope_guild` the staging-scope verdict for the invocation guild
/// (DM attempts reach here as out-of-scope). Decode refusals win over the
/// gate: they refuse before any config read.
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

/// Post-claim decision from the locked replay read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostClaimPlan {
    /// A committed row exists: render it with no limiter charge and no
    /// evaluation. `reply` is `None` for a missing/malformed row: fail closed
    /// as unavailable, never a guess or a fresh evaluation.
    Replay { reply: Option<String> },
    /// No committed row (first delivery, or a rolled-back attempt that may
    /// retry): run the limiter, then [`plan_fresh`].
    Fresh,
}

/// Replay-or-fresh from the transaction-scoped audit read (§6 redelivery).
/// `None` is a first delivery or a rolled-back attempt; `Some` is a committed
/// winner whose reply renders byte-identical from the stored row.
#[must_use]
pub fn plan_post_claim(row: Option<&PreviewAuditRow>) -> PostClaimPlan {
    match row {
        Some(row) => PostClaimPlan::Replay {
            reply: replay_preview_reply(&row.outcome, row.reason.as_deref(), &row.created_at),
        },
        None => PostClaimPlan::Fresh,
    }
}

/// Fresh-attempt decision from the limiter, after a no-row read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshPlan {
    /// Evaluate synchronously, then insert via [`evaluated_commit`]. Covers
    /// both `Admit` and `ReplayDuplicate`: the duplicate is an idempotent
    /// retry of an interaction admitted but rolled back in this limiter
    /// lifetime, so it evaluates without charging twice.
    Evaluate,
    /// Rate refused: evaluate nothing (matcher call count stays 0), insert
    /// via [`rate_limited_commit`].
    RefuseRateLimited,
}

/// Map the limiter decision to evaluation or refusal.
#[must_use]
pub fn plan_fresh(decision: PreviewRateDecision) -> FreshPlan {
    match decision {
        PreviewRateDecision::Admit | PreviewRateDecision::ReplayDuplicate => FreshPlan::Evaluate,
        PreviewRateDecision::RateLimited => FreshPlan::RefuseRateLimited,
    }
}

/// Everything the winner inserts and emits for one evaluated preview:
/// the audit record plus the metric. Built from the verdict enums and the
/// input length only — never content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewCommit {
    pub record: AuditRecord,
    pub outcome: &'static str,
    pub reason: String,
    pub metric: PreviewMetricLabels,
}

fn commit_record(
    guild_id: &str,
    actor_id: &str,
    channel_id: &str,
    outcome: &'static str,
    reason: String,
) -> AuditRecord {
    AuditRecord {
        guild_id: guild_id.to_owned(),
        actor_id: Some(actor_id.to_owned()),
        action: crate::automod_preview_audit::PREVIEW_AUDIT_ACTION.to_owned(),
        target_key: Some(channel_id.to_owned()),
        outcome: outcome.to_owned(),
        reason: Some(reason),
    }
}

/// Commit bundle for one evaluated verdict. The handler inserts `record` with
/// the winning receipt time as `created_at`, commits, then renders the first
/// reply from the stored timestamp — so it matches every replay exactly.
#[must_use]
pub fn evaluated_commit(
    verdict: &PreviewVerdict,
    text_len_utf16: u32,
    guild_id: &str,
    actor_id: &str,
    channel_id: &str,
) -> PreviewCommit {
    let outcome = preview_audit_outcome(verdict);
    let reason = audit_reason_for_verdict(verdict, text_len_utf16);
    PreviewCommit {
        record: commit_record(guild_id, actor_id, channel_id, outcome, reason.clone()),
        outcome,
        reason,
        metric: metric_for_verdict(verdict),
    }
}

/// Commit bundle for an in-scope rate refusal: nothing was evaluated, so the
/// effective mode and the input-length bucket are recorded without a filter.
#[must_use]
pub fn rate_limited_commit(
    dry_run: bool,
    text_len_utf16: u32,
    guild_id: &str,
    actor_id: &str,
    channel_id: &str,
) -> PreviewCommit {
    let outcome = "rate_limited";
    let reason = audit_reason_rate_limited(dry_run, text_len_utf16);
    PreviewCommit {
        record: commit_record(guild_id, actor_id, channel_id, outcome, reason.clone()),
        outcome,
        reason,
        metric: metric_rate_limited(dry_run),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automod::AutomodFilter;
    use crate::automod_preview::{PreviewExemption, PreviewResult};
    use crate::automod_preview_report::render_preview_reply;

    fn decoded(len: u32) -> DecodedPreviewInput {
        DecodedPreviewInput {
            text: "hello".to_owned(),
            text_len_utf16: len,
            mention_count: 0,
            attachment_names: vec![],
        }
    }

    fn all_refusals() -> Vec<PreviewInputRefusal> {
        use PreviewInputRefusal::*;
        vec![
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
        ]
    }

    #[test]
    fn decode_refusal_answers_now_with_no_metric() {
        for refusal in all_refusals() {
            let plan = plan_pre_claim(Err(refusal), true, true, "7");
            assert_eq!(
                plan,
                PreClaimPlan::AnswerNow {
                    reply: refusal.message().to_owned(),
                    metric: None,
                },
                "refusal {refusal:?} must not touch limiter, DB, or metrics"
            );
        }
    }

    #[test]
    fn gate_matrix_refuses_before_any_claim() {
        let input = decoded(9);
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

    fn matched_verdict(dry_run: bool) -> PreviewVerdict {
        PreviewVerdict {
            result: PreviewResult::Matched,
            filter: Some(AutomodFilter::BadWords),
            knobs: crate::automod_preview::deciding_knobs(AutomodFilter::BadWords),
            exemption: None,
            dry_run,
        }
    }

    fn row_for(verdict: &PreviewVerdict, len: u32, created_at: &str) -> PreviewAuditRow {
        PreviewAuditRow {
            outcome: preview_audit_outcome(verdict).to_owned(),
            reason: Some(audit_reason_for_verdict(verdict, len)),
            created_at: created_at.to_owned(),
        }
    }

    #[test]
    fn committed_row_replays_byte_identical_without_evaluating() {
        let created_at = "2026-11-02 08:00:00+00";
        let verdicts = vec![
            matched_verdict(true),
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
                knobs: crate::automod_preview::exemption_knobs(PreviewExemption::BypassRole),
                exemption: Some(PreviewExemption::BypassRole),
                dry_run: true,
            },
            PreviewVerdict::partial(true),
        ];
        for verdict in &verdicts {
            let row = row_for(verdict, 12, created_at);
            let first = render_preview_reply(verdict, created_at);
            assert_eq!(
                plan_post_claim(Some(&row)),
                PostClaimPlan::Replay { reply: Some(first) },
                "replay must equal the first reply with no evaluation"
            );
        }
        // A rate-limited winner replays the generic line, not a verdict.
        let rate_row = PreviewAuditRow {
            outcome: "rate_limited".to_owned(),
            reason: Some(audit_reason_rate_limited(false, 600)),
            created_at: created_at.to_owned(),
        };
        assert_eq!(
            plan_post_claim(Some(&rate_row)),
            PostClaimPlan::Replay {
                reply: Some(crate::automod_preview_report::PREVIEW_RATE_LIMITED_REPLY.to_owned()),
            }
        );
    }

    #[test]
    fn malformed_row_replays_unavailable_and_no_row_goes_fresh() {
        let created_at = "2026-11-02 08:00:00+00";
        for row in [
            PreviewAuditRow {
                outcome: "bogus".to_owned(),
                reason: Some("filter=none;exemption=none;mode=dry_run;len=0_50".to_owned()),
                created_at: created_at.to_owned(),
            },
            PreviewAuditRow {
                outcome: "matched".to_owned(),
                reason: Some("filter=nonsense;exemption=none;mode=dry_run;len=0_50".to_owned()),
                created_at: created_at.to_owned(),
            },
            PreviewAuditRow {
                outcome: "no_match".to_owned(),
                reason: None,
                created_at: created_at.to_owned(),
            },
            PreviewAuditRow {
                outcome: "no_match".to_owned(),
                reason: Some("filter=none;exemption=none;mode=dry_run;len=0_50".to_owned()),
                created_at: String::new(),
            },
        ] {
            assert_eq!(
                plan_post_claim(Some(&row)),
                PostClaimPlan::Replay { reply: None },
                "malformed row {row:?} must fail closed, never guess or re-evaluate"
            );
        }
        // No row is a first delivery or a rolled-back attempt: retry, never replay.
        assert_eq!(plan_post_claim(None), PostClaimPlan::Fresh);
    }

    #[test]
    fn limiter_retry_evaluates_without_double_charge_and_refusal_evaluates_nothing() {
        use PreviewRateDecision::*;
        assert_eq!(plan_fresh(Admit), FreshPlan::Evaluate);
        assert_eq!(
            plan_fresh(ReplayDuplicate),
            FreshPlan::Evaluate,
            "idempotent retry evaluates without charging twice"
        );
        assert_eq!(plan_fresh(RateLimited), FreshPlan::RefuseRateLimited);
    }

    #[test]
    fn commit_bundles_record_outcome_reason_and_metric() {
        let verdict = matched_verdict(true);
        let commit = evaluated_commit(&verdict, 12, "guild-1", "actor-1", "channel-1");
        assert_eq!(commit.outcome, "matched");
        assert_eq!(
            commit.reason,
            "filter=bad_words;exemption=none;mode=dry_run;len=0_50"
        );
        assert_eq!(commit.metric, metric_for_verdict(&verdict));
        assert_eq!(commit.record.guild_id, "guild-1");
        assert_eq!(commit.record.actor_id.as_deref(), Some("actor-1"));
        assert_eq!(
            commit.record.action,
            crate::automod_preview_audit::PREVIEW_AUDIT_ACTION
        );
        assert_eq!(commit.record.target_key.as_deref(), Some("channel-1"));
        assert_eq!(commit.record.outcome, "matched");
        assert_eq!(
            commit.record.reason.as_deref(),
            Some(commit.reason.as_str())
        );

        let limited = rate_limited_commit(false, 600, "guild-1", "actor-1", "channel-1");
        assert_eq!(limited.outcome, "rate_limited");
        assert_eq!(
            limited.reason,
            "filter=none;exemption=none;mode=enforce;len=501_2000"
        );
        assert_eq!(limited.metric, metric_rate_limited(false));
        assert_eq!(limited.record.outcome, "rate_limited");
    }
}
