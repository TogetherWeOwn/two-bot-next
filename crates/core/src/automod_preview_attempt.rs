//! Drive one authorized preview attempt without sending a Discord response.

use std::future::Future;

use crate::automod_preview::PreviewVerdict;
use crate::automod_preview_audit::PreviewAuditRow;
use crate::automod_preview_flow::{
    evaluated_commit, plan_fresh, plan_post_claim, rate_limited_commit, FreshPlan, PostClaimPlan,
    PreClaimPlan,
};
use crate::automod_preview_limiter::PreviewRateDecision;
use crate::automod_preview_report::{
    metric_unavailable, render_preview_reply, PreviewMetricLabels, PREVIEW_RATE_LIMITED_REPLY,
    PREVIEW_UNAVAILABLE_REPLY,
};
use crate::custom_commands::AuditRecord;

/// One transaction per claim. An adapter must bound database waits and release
/// the claim if dropped before commit. Store errors are not rendered or logged.
pub trait PreviewAttemptStore {
    type Error;

    /// Acquire the claim and read on the same transaction at READ COMMITTED.
    fn claim_and_read(
        &mut self,
        audit_id: &str,
    ) -> impl Future<Output = Result<Option<PreviewAuditRow>, Self::Error>> + Send;

    /// Insert with the winning receipt time and return the stored timestamp
    /// only after commit succeeds. An unknown commit outcome is an error.
    fn insert_and_commit(
        self,
        record: AuditRecord,
        audit_id: &str,
        receipt_iso: &str,
    ) -> impl Future<Output = Result<String, Self::Error>> + Send;
}

/// Content-free context captured by the caller for a fresh attempt.
pub struct AttemptContext<'a> {
    pub guild_id: &'a str,
    pub actor_id: &'a str,
    pub channel_id: &'a str,
    pub receipt_iso: &'a str,
    pub dry_run: bool,
}

/// The caller sends the reply privately and emits the metric, if any.
pub struct AttemptResult {
    pub reply: String,
    pub metric: Option<PreviewMetricLabels>,
}

/// Proceed from the pre-claim plan. Only a successful locked miss can charge
/// or evaluate. Acquire and release shared guards inside the synchronous
/// callbacks, so neither callback holds a guard during audit I/O.
pub async fn attempt_preview(
    plan: PreClaimPlan,
    context: AttemptContext<'_>,
    mut store: impl PreviewAttemptStore,
    admit: impl FnOnce() -> PreviewRateDecision,
    evaluate: impl FnOnce() -> PreviewVerdict,
) -> AttemptResult {
    let (audit_id, len) = match plan {
        PreClaimPlan::AnswerNow { reply, metric } => return AttemptResult { reply, metric },
        PreClaimPlan::Claim {
            audit_id,
            text_len_utf16,
        } => (audit_id, text_len_utf16),
    };
    let AttemptContext {
        guild_id,
        actor_id,
        channel_id,
        receipt_iso,
        dry_run,
    } = context;
    let unavailable = || AttemptResult {
        reply: PREVIEW_UNAVAILABLE_REPLY.to_owned(),
        metric: Some(metric_unavailable(Some(dry_run))),
    };
    let Ok(row) = store.claim_and_read(&audit_id).await else {
        return unavailable();
    };
    match plan_post_claim(row.as_ref()) {
        PostClaimPlan::Replay { reply: Some(reply) } => {
            return AttemptResult {
                reply,
                metric: None,
            }
        }
        PostClaimPlan::Replay { reply: None } => return unavailable(),
        PostClaimPlan::Fresh => {}
    }
    let (commit, verdict) = match plan_fresh(admit()) {
        FreshPlan::Evaluate => {
            let verdict = evaluate();
            (
                evaluated_commit(&verdict, len, guild_id, actor_id, channel_id),
                Some(verdict),
            )
        }
        FreshPlan::RefuseRateLimited => (
            rate_limited_commit(dry_run, len, guild_id, actor_id, channel_id),
            None,
        ),
    };
    let Ok(stored_at) = store
        .insert_and_commit(commit.record, &audit_id, receipt_iso)
        .await
    else {
        return unavailable();
    };
    AttemptResult {
        reply: verdict.map_or_else(
            || PREVIEW_RATE_LIMITED_REPLY.to_owned(),
            |verdict| render_preview_reply(&verdict, &stored_at),
        ),
        metric: Some(commit.metric),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automod_preview::PreviewResult;
    use crate::automod_preview_report::{
        audit_reason_for_verdict, metric_for_verdict, metric_rate_limited,
    };
    use std::cell::Cell;
    use std::future::ready;
    use std::task::{Context, Poll, Waker};
    use tokio::sync::oneshot;

    const CONTEXT: AttemptContext<'static> = AttemptContext {
        guild_id: "guild",
        actor_id: "actor",
        channel_id: "channel",
        receipt_iso: "receipt",
        dry_run: true,
    };
    const STORED_AT: &str = "2026-11-19T11:00:00.000000Z";

    struct Store<F> {
        row: Result<Option<PreviewAuditRow>, ()>,
        commit: F,
    }

    impl<F, Fut> PreviewAttemptStore for Store<F>
    where
        F: FnOnce(AuditRecord) -> Fut,
        Fut: Future<Output = Result<String, ()>> + Send,
    {
        type Error = ();

        fn claim_and_read(
            &mut self,
            id: &str,
        ) -> impl Future<Output = Result<Option<PreviewAuditRow>, ()>> + Send {
            assert_eq!(id, "claim");
            ready(std::mem::replace(&mut self.row, Err(())))
        }

        fn insert_and_commit(
            self,
            record: AuditRecord,
            id: &str,
            receipt: &str,
        ) -> impl Future<Output = Result<String, ()>> + Send {
            assert_eq!((id, receipt), ("claim", "receipt"));
            (self.commit)(record)
        }
    }

    fn claim() -> PreClaimPlan {
        PreClaimPlan::Claim {
            audit_id: "claim".into(),
            text_len_utf16: 5,
        }
    }

    fn verdict() -> PreviewVerdict {
        PreviewVerdict {
            result: PreviewResult::NoMatch,
            filter: None,
            knobs: &[],
            exemption: None,
            dry_run: true,
        }
    }

    #[tokio::test]
    async fn fresh_verdict_waits_for_commit_and_uses_stored_time() {
        for committed in [true, false] {
            let (tx, rx) = oneshot::channel();
            let commit = |record: AuditRecord| {
                assert_eq!(record.outcome, "no_match");
                assert_eq!(record.reason, Some(audit_reason_for_verdict(&verdict(), 5)));
                async { rx.await.map_err(|_| ()) }
            };
            let store = Store {
                row: Ok(None),
                commit,
            };
            let mut attempt = std::pin::pin!(attempt_preview(
                claim(),
                CONTEXT,
                store,
                || PreviewRateDecision::ReplayDuplicate,
                verdict
            ));
            assert!(matches!(
                attempt
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            ));
            if committed {
                tx.send(STORED_AT.to_owned())
                    .expect("commit receiver remains pending");
            } else {
                drop(tx);
            }
            let result = attempt.await;
            if committed {
                assert_eq!(result.reply, render_preview_reply(&verdict(), STORED_AT));
                assert_eq!(result.metric, Some(metric_for_verdict(&verdict())));
            } else {
                assert_eq!(result.reply, PREVIEW_UNAVAILABLE_REPLY);
                assert_eq!(result.metric, Some(metric_unavailable(Some(true))));
            }
        }
    }

    #[tokio::test]
    async fn locked_reads_and_refusals_do_not_evaluate() {
        let mut row = PreviewAuditRow {
            outcome: "no_match".into(),
            reason: Some(audit_reason_for_verdict(&verdict(), 5)),
            created_at: STORED_AT.into(),
        };
        let replay = Ok(Some(row.clone()));
        row.reason = None;
        for (row, expected, metric, fresh) in [
            (
                replay,
                render_preview_reply(&verdict(), STORED_AT),
                None,
                false,
            ),
            (
                Ok(Some(row)),
                PREVIEW_UNAVAILABLE_REPLY.into(),
                Some(metric_unavailable(Some(true))),
                false,
            ),
            (
                Err(()),
                PREVIEW_UNAVAILABLE_REPLY.into(),
                Some(metric_unavailable(Some(true))),
                false,
            ),
            (
                Ok(None),
                PREVIEW_RATE_LIMITED_REPLY.into(),
                Some(metric_rate_limited(true)),
                true,
            ),
        ] {
            let (charged, evaluated, written) =
                (Cell::new(false), Cell::new(false), Cell::new(false));
            let commit = |record: AuditRecord| {
                written.set(true);
                assert_eq!(record.outcome, "rate_limited");
                ready(Ok(STORED_AT.into()))
            };
            let admit = || {
                charged.set(true);
                PreviewRateDecision::RateLimited
            };
            let evaluate = || {
                evaluated.set(true);
                verdict()
            };
            let result =
                attempt_preview(claim(), CONTEXT, Store { row, commit }, admit, evaluate).await;
            assert_eq!((result.reply, result.metric), (expected, metric));
            assert_eq!(
                (charged.get(), evaluated.get(), written.get()),
                (fresh, false, fresh)
            );
        }
    }
}
