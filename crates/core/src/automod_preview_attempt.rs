//! Commit a fresh preview attempt before preparing a reply.

use std::future::Future;

use crate::automod_preview::PreviewVerdict;
use crate::automod_preview_audit::replay_preview_reply;
use crate::automod_preview_flow::{evaluated_commit, rate_limited_commit, FreshPlan};
use crate::automod_preview_report::{
    metric_unavailable, PreviewMetricLabels, PREVIEW_UNAVAILABLE_REPLY,
};
use crate::custom_commands::AuditRecord;

/// One transaction per claim. An adapter must bound database waits and release
/// the claim if dropped before commit. Store errors are not rendered or logged.
pub trait PreviewAttemptStore {
    type Error;

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
    /// Mode for rate refusals; evaluated attempts use the verdict's mode.
    pub dry_run: bool,
}

/// The caller sends the reply privately and emits the metric, if any.
pub struct AttemptResult {
    pub reply: String,
    pub metric: Option<PreviewMetricLabels>,
}

fn unavailable(dry_run: bool) -> AttemptResult {
    AttemptResult {
        reply: PREVIEW_UNAVAILABLE_REPLY.to_owned(),
        metric: Some(metric_unavailable(Some(dry_run))),
    }
}

/// Complete a fresh attempt after the adapter has claimed and read no row.
/// The caller supplies the admission decision and must release the limiter
/// guard before calling this function. Renders from the same audit fields as
/// replay. Returns no verdict if the commit fails or its row cannot be replayed.
pub async fn finish_fresh_attempt(
    plan: FreshPlan,
    context: AttemptContext<'_>,
    audit_id: &str,
    len: u32,
    store: impl PreviewAttemptStore,
    evaluate: impl FnOnce() -> PreviewVerdict,
) -> AttemptResult {
    let AttemptContext {
        guild_id,
        actor_id,
        channel_id,
        receipt_iso,
        dry_run,
    } = context;
    let (commit, effective_dry_run) = match plan {
        FreshPlan::Evaluate => {
            let verdict = evaluate();
            (
                evaluated_commit(&verdict, len, guild_id, actor_id, channel_id),
                verdict.dry_run,
            )
        }
        FreshPlan::RefuseRateLimited => (
            rate_limited_commit(dry_run, len, guild_id, actor_id, channel_id),
            dry_run,
        ),
    };
    let outcome = commit.record.outcome.clone();
    let reason = commit.record.reason.clone();
    let Ok(stored_at) = store
        .insert_and_commit(commit.record, audit_id, receipt_iso)
        .await
    else {
        return unavailable(effective_dry_run);
    };
    let Some(reply) = replay_preview_reply(&outcome, reason.as_deref(), &stored_at) else {
        return unavailable(effective_dry_run);
    };
    AttemptResult {
        reply,
        metric: Some(commit.metric),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automod::AutomodFilter;
    use crate::automod_preview::PreviewResult;
    use crate::automod_preview_report::{
        metric_for_verdict, metric_rate_limited, render_preview_reply, PREVIEW_RATE_LIMITED_REPLY,
    };
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
        commit: F,
    }

    impl<F, Fut> PreviewAttemptStore for Store<F>
    where
        F: FnOnce(AuditRecord) -> Fut,
        Fut: Future<Output = Result<String, ()>> + Send,
    {
        type Error = ();

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
    async fn fresh_reply_matches_replay_of_committed_row() {
        let mut committed = None;
        let commit = |record: AuditRecord| {
            committed = Some(record);
            std::future::ready(Ok(STORED_AT.to_owned()))
        };
        let evaluate = || PreviewVerdict {
            result: PreviewResult::Matched,
            filter: Some(AutomodFilter::BadWords),
            knobs: &[],
            ..verdict()
        };
        let result = finish_fresh_attempt(
            FreshPlan::Evaluate,
            CONTEXT,
            "claim",
            5,
            Store { commit },
            evaluate,
        )
        .await;
        let record = committed.expect("audit row committed");
        let replay = replay_preview_reply(&record.outcome, record.reason.as_deref(), STORED_AT)
            .expect("committed row can be replayed");
        assert!(replay.contains("TWO_AUTOMOD_BAD_WORDS"));
        assert_eq!(result.reply, replay);
    }

    #[tokio::test]
    async fn fresh_reply_waits_for_commit_and_uses_stored_time() {
        for (plan, stored_at) in [
            (FreshPlan::Evaluate, Some(STORED_AT)),
            (FreshPlan::Evaluate, None),
            (FreshPlan::Evaluate, Some("")),
            (FreshPlan::RefuseRateLimited, Some(STORED_AT)),
            (FreshPlan::RefuseRateLimited, None),
        ] {
            let evaluated = matches!(plan, FreshPlan::Evaluate);
            let (tx, rx) = oneshot::channel();
            let commit = |record: AuditRecord| {
                let expected = if evaluated {
                    evaluated_commit(&verdict(), 5, "guild", "actor", "channel")
                } else {
                    rate_limited_commit(true, 5, "guild", "actor", "channel")
                };
                assert_eq!(record, expected.record);
                async { rx.await.map_err(|_| ()) }
            };
            // An evaluated attempt uses its verdict mode even when the context differs.
            let context = AttemptContext {
                dry_run: !evaluated,
                ..CONTEXT
            };
            let mut attempt = std::pin::pin!(finish_fresh_attempt(
                plan,
                context,
                "claim",
                5,
                Store { commit },
                verdict
            ));
            assert!(matches!(
                attempt
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            ));
            if let Some(stored_at) = stored_at {
                tx.send(stored_at.to_owned())
                    .expect("commit receiver remains pending");
            } else {
                drop(tx);
            }
            let result = attempt.await;
            if stored_at == Some(STORED_AT) && evaluated {
                assert_eq!(result.reply, render_preview_reply(&verdict(), STORED_AT));
                assert_eq!(result.metric, Some(metric_for_verdict(&verdict())));
            } else if stored_at == Some(STORED_AT) {
                assert_eq!(result.reply, PREVIEW_RATE_LIMITED_REPLY);
                assert_eq!(result.metric, Some(metric_rate_limited(true)));
            } else {
                assert_eq!(result.reply, PREVIEW_UNAVAILABLE_REPLY);
                assert_eq!(result.metric, Some(metric_unavailable(Some(true))));
            }
        }
    }
}
