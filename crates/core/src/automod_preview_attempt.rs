//! One async preview attempt: the §6 protocol as code
//! (docs/automod-preview-spec.md §6; slice 3g).
//!
//! The Discord layer owns the twilight option mapping, the runtime snapshot,
//! and the sqlx transaction; it calls [`attempt_preview`] with those pieces
//! so the redelivery protocol lives in one reviewable function, not prose:
//!
//! 1. [`plan_pre_claim`](crate::automod_preview_flow::plan_pre_claim): strict
//!    decode and scope gate first. Either refusal answers immediately — no
//!    lock, no limiter charge, no audit row, and the store is never touched.
//! 2. [`claim_and_read`](PreviewAttemptStore::claim_and_read): the winner
//!    takes the transaction-scoped advisory lock and reads the audit id on
//!    that same connection. Any store error fails closed as unavailable with
//!    `metric_unavailable` — never an unlocked evaluation.
//! 3. [`plan_post_claim`](crate::automod_preview_flow::plan_post_claim): a
//!    committed row replays byte-identical with no limiter charge, no
//!    evaluation, and no new metric (the winner already counted it); no row
//!    goes fresh.
//! 4. [`plan_fresh`](crate::automod_preview_flow::plan_fresh): only
//!    `RateLimited` refuses, evaluating nothing; `Admit` and the idempotent
//!    `ReplayDuplicate` evaluate exactly once via the caller's closure.
//! 5. The winner inserts the commit bundle and commits **before any verdict
//!    reply**, then renders the first reply from the *stored* timestamp — so
//!    it matches every replay at database precision. Insert failure
//!    suppresses the verdict (generic unavailable); the transaction rolls
//!    back on drop and a later delivery may retry with no stranded claim.
//!
//! The `evaluate` closure must be built from one cloned effective config plus
//! a synchronous lock-scoped read (never held across an await): this driver
//! awaits only the store, so the runtime mutex is free before any audit I/O
//! by construction. Nothing here takes the previewed text — only the decoded
//! input, verdict enums, and timestamp text — so content cannot leak.

use crate::automod_preview::PreviewVerdict;
use crate::automod_preview_audit::PreviewAuditRow;
use crate::automod_preview_flow::{
    evaluated_commit, plan_fresh, plan_post_claim, plan_pre_claim, rate_limited_commit, FreshPlan,
    PostClaimPlan,
};
use crate::automod_preview_input::{DecodedPreviewInput, PreviewInputRefusal};
use crate::automod_preview_limiter::PreviewRateLimiter;
use crate::automod_preview_report::{
    metric_unavailable, render_preview_reply, PreviewMetricLabels, PREVIEW_RATE_LIMITED_REPLY,
    PREVIEW_UNAVAILABLE_REPLY,
};
use crate::custom_commands::AuditRecord;

/// Transaction the driver runs one attempt against. The real implementation
/// (discord layer, `db` feature) wraps the winner's sqlx transaction:
/// `claim_and_read` takes the advisory lock and reads on that same
/// connection; `insert_and_commit` inserts and commits, so commit and result
/// are never separated. Rollback on drop releases the claim with no stranded
/// row. Fakes prove the protocol offline.
pub trait PreviewAttemptStore {
    /// Failure of one store call. Any value fails the attempt closed as
    /// unavailable; it is never rendered, logged with content, or retried
    /// outside a fresh locked read.
    type Error: core::fmt::Display;

    /// Take the transaction-scoped advisory lock for `audit_id`, then read
    /// that id on the same connection. `Ok(None)` is a first delivery or a
    /// rolled-back attempt that may retry; `Ok(Some)` is a committed winner
    /// to replay without evaluating.
    fn claim_and_read(
        &mut self,
        audit_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<PreviewAuditRow>, Self::Error>> + Send;

    /// Insert one content-free audit row with `created_at = receipt_iso` and
    /// commit. Returns the stored timestamp text, from which the caller
    /// renders the first reply so it matches every replay exactly.
    fn insert_and_commit(
        self,
        record: AuditRecord,
        audit_id: &str,
        receipt_iso: &str,
    ) -> impl std::future::Future<Output = Result<String, Self::Error>> + Send;
}

/// Everything the driver needs that is not the store, the limiter, or the
/// evaluator. `dry_run` is the effective-mode snapshot the caller captured
/// synchronously before the claim; when no runtime exists despite the gate
/// passing, the caller answers unavailable without calling the driver.
pub struct AttemptInput<'a> {
    pub decoded: Result<&'a DecodedPreviewInput, PreviewInputRefusal>,
    pub automod_enabled: bool,
    pub in_scope_guild: bool,
    pub guild_id: &'a str,
    pub actor_id: &'a str,
    pub channel_id: &'a str,
    pub interaction_id: &'a str,
    /// Numeric interaction id for the limiter's idempotency set.
    pub interaction_numeric: u64,
    /// Winning attempt's captured UTC receipt time: the committed
    /// `created_at`, never a database clock.
    pub receipt_iso: &'a str,
    /// Limiter clock (ms).
    pub now_ms: u64,
    pub dry_run: bool,
}

/// One settled attempt: the ephemeral reply text plus the single metric
/// sample for it, if any. Replays and decode refusals carry no metric — the
/// winner already counted its attempt, and a refusal evaluated nothing.
pub struct AttemptResult {
    pub reply: String,
    pub metric: Option<PreviewMetricLabels>,
    /// Whether the `evaluate` closure ran. Test hook and audit-trail proof
    /// that refusals and replays evaluate nothing; true even when an insert
    /// failure suppressed the verdict, since failed attempts may evaluate
    /// more than once (§6).
    pub evaluated: bool,
}

fn unavailable(dry_run: bool) -> AttemptResult {
    AttemptResult {
        reply: PREVIEW_UNAVAILABLE_REPLY.to_owned(),
        metric: Some(metric_unavailable(Some(dry_run))),
        evaluated: false,
    }
}

/// Run one §6 attempt. See the module docs for the order; each step maps to
/// one [`crate::automod_preview_flow`] plan so the protocol stays in code.
pub async fn attempt_preview<S>(
    input: AttemptInput<'_>,
    limiter: &mut PreviewRateLimiter,
    store: S,
    evaluate: impl FnOnce() -> PreviewVerdict,
) -> AttemptResult
where
    S: PreviewAttemptStore,
{
    let claim = match plan_pre_claim(
        input.decoded,
        input.automod_enabled,
        input.in_scope_guild,
        input.interaction_id,
    ) {
        crate::automod_preview_flow::PreClaimPlan::AnswerNow { reply, metric } => {
            return AttemptResult {
                reply,
                metric,
                evaluated: false,
            };
        }
        crate::automod_preview_flow::PreClaimPlan::Claim {
            audit_id,
            text_len_utf16,
        } => (audit_id, text_len_utf16),
    };
    let (audit_id, text_len_utf16) = claim;
    let mut store = store;
    let row = match store.claim_and_read(&audit_id).await {
        Ok(row) => row,
        Err(_) => return unavailable(input.dry_run),
    };
    match plan_post_claim(row.as_ref()) {
        PostClaimPlan::Replay { reply: Some(reply) } => {
            return AttemptResult {
                reply,
                metric: None,
                evaluated: false,
            };
        }
        PostClaimPlan::Replay { reply: None } => return unavailable(input.dry_run),
        PostClaimPlan::Fresh => {}
    }
    let decision = limiter.check_and_admit(
        input.actor_id,
        input.guild_id,
        input.interaction_numeric,
        input.now_ms,
    );
    match plan_fresh(decision) {
        FreshPlan::Evaluate => {
            let verdict = evaluate();
            let commit = evaluated_commit(
                &verdict,
                text_len_utf16,
                input.guild_id,
                input.actor_id,
                input.channel_id,
            );
            match store
                .insert_and_commit(commit.record, &audit_id, input.receipt_iso)
                .await
            {
                Ok(stored_at) => AttemptResult {
                    reply: render_preview_reply(&verdict, &stored_at),
                    metric: Some(commit.metric),
                    evaluated: true,
                },
                // The verdict ran but is suppressed: report that it ran
                // (failed attempts may evaluate more than once, §6) while
                // exposing nothing of it.
                Err(_) => AttemptResult {
                    reply: PREVIEW_UNAVAILABLE_REPLY.to_owned(),
                    metric: Some(metric_unavailable(Some(input.dry_run))),
                    evaluated: true,
                },
            }
        }
        FreshPlan::RefuseRateLimited => {
            let commit = rate_limited_commit(
                input.dry_run,
                text_len_utf16,
                input.guild_id,
                input.actor_id,
                input.channel_id,
            );
            match store
                .insert_and_commit(commit.record, &audit_id, input.receipt_iso)
                .await
            {
                Ok(_) => AttemptResult {
                    reply: PREVIEW_RATE_LIMITED_REPLY.to_owned(),
                    metric: Some(commit.metric),
                    evaluated: false,
                },
                Err(_) => unavailable(input.dry_run),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automod::AutomodFilter;
    use crate::automod_preview::{deciding_knobs, PreviewResult};
    use crate::automod_preview_report::{audit_reason_for_verdict, preview_audit_outcome};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Default)]
    struct Calls {
        claims: Vec<String>,
        inserts: Vec<(AuditRecord, String, String)>,
    }

    /// Fake store: scripted read, scripted insert result, shared call log.
    /// `insert_and_commit` takes `self` by value like a real transaction, so
    /// the log lives behind `Arc<Mutex<..>>`.
    struct FakeStore {
        row: Option<PreviewAuditRow>,
        insert: Result<String, String>,
        fail_claim: Option<String>,
        calls: Arc<Mutex<Calls>>,
    }

    impl FakeStore {
        fn fresh(row: Option<PreviewAuditRow>, stored_at: &str) -> (Self, Arc<Mutex<Calls>>) {
            let calls = Arc::new(Mutex::new(Calls::default()));
            (
                Self {
                    row,
                    insert: Ok(stored_at.to_owned()),
                    fail_claim: None,
                    calls: Arc::clone(&calls),
                },
                calls,
            )
        }
    }

    impl PreviewAttemptStore for FakeStore {
        type Error = String;

        fn claim_and_read(
            &mut self,
            audit_id: &str,
        ) -> impl std::future::Future<Output = Result<Option<PreviewAuditRow>, Self::Error>> + Send
        {
            let audit_id = audit_id.to_owned();
            let row = self.row.clone();
            let fail_claim = self.fail_claim.clone();
            let calls = Arc::clone(&self.calls);
            async move {
                calls.lock().expect("call log").claims.push(audit_id);
                if let Some(error) = fail_claim {
                    return Err(error);
                }
                Ok(row)
            }
        }

        fn insert_and_commit(
            self,
            record: AuditRecord,
            audit_id: &str,
            receipt_iso: &str,
        ) -> impl std::future::Future<Output = Result<String, Self::Error>> + Send {
            let logged = (record, audit_id.to_owned(), receipt_iso.to_owned());
            let insert = self.insert.clone();
            let calls = self.calls;
            async move {
                calls.lock().expect("call log").inserts.push(logged);
                insert
            }
        }
    }

    fn input<'a>(
        decoded: Result<&'a DecodedPreviewInput, PreviewInputRefusal>,
    ) -> AttemptInput<'a> {
        AttemptInput {
            decoded,
            automod_enabled: true,
            in_scope_guild: true,
            guild_id: "guild-1",
            actor_id: "actor-1",
            channel_id: "channel-1",
            interaction_id: "7",
            interaction_numeric: 7,
            receipt_iso: "2026-11-02 08:00:00+00",
            now_ms: 1_000_000,
            dry_run: true,
        }
    }

    fn decoded(len: u32) -> DecodedPreviewInput {
        DecodedPreviewInput {
            text: "hello".to_owned(),
            text_len_utf16: len,
            mention_count: 0,
            attachment_names: vec![],
        }
    }

    fn matched_verdict() -> PreviewVerdict {
        PreviewVerdict {
            result: PreviewResult::Matched,
            filter: Some(AutomodFilter::BadWords),
            knobs: deciding_knobs(AutomodFilter::BadWords),
            exemption: None,
            dry_run: true,
        }
    }

    fn must_not_evaluate() -> impl FnOnce() -> PreviewVerdict {
        || panic!("refusals and replays must evaluate nothing")
    }

    #[tokio::test]
    async fn decode_refusal_never_touches_store_limiter_or_metrics() {
        let refused = decoded(9);
        let (store, calls) = FakeStore::fresh(None, "2026-11-02 08:00:00+00");
        let mut limiter = PreviewRateLimiter::new();
        let result = attempt_preview(
            input(Err(PreviewInputRefusal::TextBounds)),
            &mut limiter,
            store,
            must_not_evaluate(),
        )
        .await;
        assert_eq!(result.reply, PreviewInputRefusal::TextBounds.message());
        assert_eq!(result.metric, None);
        assert!(!result.evaluated);
        let calls = calls.lock().expect("call log");
        assert!(calls.claims.is_empty() && calls.inserts.is_empty());
        drop(refused);
    }

    #[tokio::test]
    async fn gate_refusal_answers_not_evaluated_without_a_row() {
        let gate = decoded(9);
        let (store, calls) = FakeStore::fresh(None, "2026-11-02 08:00:00+00");
        let mut limiter = PreviewRateLimiter::new();
        let mut attempt = input(Ok(&gate));
        attempt.automod_enabled = false;
        let result = attempt_preview(attempt, &mut limiter, store, must_not_evaluate()).await;
        assert_eq!(
            result.reply,
            crate::automod_preview_report::PREVIEW_NOT_EVALUATED_REPLY
        );
        assert_eq!(
            result.metric,
            Some(crate::automod_preview_report::metric_not_evaluated())
        );
        assert!(!result.evaluated);
        let calls = calls.lock().expect("call log");
        assert!(calls.claims.is_empty() && calls.inserts.is_empty());
    }

    #[tokio::test]
    async fn fresh_evaluation_renders_the_stored_timestamp() {
        let fresh = decoded(12);
        // The store commits at database precision, later than receipt: the
        // first reply must carry the stored text so replays match exactly.
        let stored_at = "2026-11-02 08:00:00.123456+00";
        let (store, calls) = FakeStore::fresh(None, stored_at);
        let mut limiter = PreviewRateLimiter::new();
        let verdict = matched_verdict();
        let expected = render_preview_reply(&verdict, stored_at);
        let result = attempt_preview(input(Ok(&fresh)), &mut limiter, store, || verdict).await;
        assert!(result.evaluated);
        assert_eq!(result.reply, expected);
        assert_eq!(
            result.metric,
            Some(crate::automod_preview_report::metric_for_verdict(
                &matched_verdict()
            ))
        );
        let calls = calls.lock().expect("call log");
        assert_eq!(calls.claims, vec!["automod-preview:7".to_owned()]);
        assert_eq!(calls.inserts.len(), 1);
        let (record, audit_id, receipt_iso) = &calls.inserts[0];
        assert_eq!(audit_id, "automod-preview:7");
        assert_eq!(receipt_iso, "2026-11-02 08:00:00+00");
        assert_eq!(record.outcome, "matched");
        assert_eq!(
            record.reason.as_deref(),
            Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50")
        );
    }

    #[tokio::test]
    async fn committed_row_replays_with_no_charge_evaluation_or_metric() {
        let verdict = matched_verdict();
        let created_at = "2026-11-02 08:00:00+00";
        let row = PreviewAuditRow {
            outcome: preview_audit_outcome(&verdict).to_owned(),
            reason: Some(audit_reason_for_verdict(&verdict, 12)),
            created_at: created_at.to_owned(),
        };
        let (store, calls) = FakeStore::fresh(Some(row), created_at);
        // Saturate the limiter first: a replay must still succeed, proving the
        // replay path charges nothing and consults no budget.
        let mut limiter = PreviewRateLimiter::new();
        limiter.check_and_admit("actor-1", "guild-1", 6, 1_000_000);
        let replaying = decoded(12);
        let result = attempt_preview(
            input(Ok(&replaying)),
            &mut limiter,
            store,
            must_not_evaluate(),
        )
        .await;
        assert!(!result.evaluated);
        assert_eq!(result.reply, render_preview_reply(&verdict, created_at));
        assert_eq!(result.metric, None);
        let calls = calls.lock().expect("call log");
        assert_eq!(calls.claims.len(), 1);
        assert!(calls.inserts.is_empty());
    }

    #[tokio::test]
    async fn rate_refusal_inserts_content_free_row_and_evaluates_nothing() {
        let limited = decoded(600);
        let (store, calls) = FakeStore::fresh(None, "2026-11-02 08:00:00+00");
        let mut limiter = PreviewRateLimiter::new();
        // Min-interval saturation: the second attempt inside 5 s refuses.
        limiter.check_and_admit("actor-1", "guild-1", 6, 1_000_000);
        let result = attempt_preview(
            input(Ok(&limited)),
            &mut limiter,
            store,
            must_not_evaluate(),
        )
        .await;
        assert!(!result.evaluated);
        assert_eq!(result.reply, PREVIEW_RATE_LIMITED_REPLY);
        assert_eq!(
            result.metric,
            Some(crate::automod_preview_report::metric_rate_limited(true))
        );
        let calls = calls.lock().expect("call log");
        assert_eq!(calls.inserts.len(), 1);
        let (record, _, _) = &calls.inserts[0];
        assert_eq!(record.outcome, "rate_limited");
        assert_eq!(
            record.reason.as_deref(),
            Some("filter=none;exemption=none;mode=dry_run;len=501_2000")
        );
    }

    #[tokio::test]
    async fn idempotent_retry_evaluates_once_after_rollback() {
        // The same interaction admitted, rolled back (no row), and retried:
        // `ReplayDuplicate` evaluates without a double charge or insert.
        let retry = decoded(12);
        let (store, calls) = FakeStore::fresh(None, "2026-11-02 08:00:00+00");
        let mut limiter = PreviewRateLimiter::new();
        limiter.check_and_admit("actor-1", "guild-1", 7, 1_000_000);
        let verdict = matched_verdict();
        let result = attempt_preview(input(Ok(&retry)), &mut limiter, store, || verdict).await;
        assert!(result.evaluated);
        assert!(result.reply.contains("bad_words"));
        let calls = calls.lock().expect("call log");
        assert_eq!(calls.inserts.len(), 1);
    }

    #[tokio::test]
    async fn store_failures_fail_closed_with_unavailable() {
        // Claim failure: no evaluation, no insert, mode-carrying metric.
        let gated = decoded(12);
        let (mut store, calls) = FakeStore::fresh(None, "2026-11-02 08:00:00+00");
        store.fail_claim = Some("lock timeout".to_owned());
        let mut limiter = PreviewRateLimiter::new();
        let result =
            attempt_preview(input(Ok(&gated)), &mut limiter, store, must_not_evaluate()).await;
        assert!(!result.evaluated);
        assert_eq!(result.reply, PREVIEW_UNAVAILABLE_REPLY);
        assert_eq!(result.metric, Some(metric_unavailable(Some(true))));
        assert!(calls.lock().expect("call log").inserts.is_empty());

        // Insert failure after evaluation: the verdict is suppressed, never
        // exposed; the rolled-back transaction may retry later.
        let evaluated = decoded(12);
        let (mut store, calls) = FakeStore::fresh(None, "2026-11-02 08:00:00+00");
        store.insert = Err("connection lost".to_owned());
        let mut limiter = PreviewRateLimiter::new();
        let result =
            attempt_preview(input(Ok(&evaluated)), &mut limiter, store, matched_verdict).await;
        assert!(
            result.evaluated,
            "evaluation ran but its verdict was suppressed"
        );
        assert_eq!(result.reply, PREVIEW_UNAVAILABLE_REPLY);
        assert!(!result.reply.contains("bad_words"));
        assert_eq!(result.metric, Some(metric_unavailable(Some(true))));
        assert_eq!(calls.lock().expect("call log").inserts.len(), 1);

        // Malformed committed row: locked replay read fails closed, never a
        // guess or a fresh evaluation.
        let malformed = decoded(12);
        let row = PreviewAuditRow {
            outcome: "bogus".to_owned(),
            reason: Some("filter=none;exemption=none;mode=dry_run;len=0_50".to_owned()),
            created_at: "2026-11-02 08:00:00+00".to_owned(),
        };
        let (store, _) = FakeStore::fresh(Some(row), "2026-11-02 08:00:00+00");
        let mut limiter = PreviewRateLimiter::new();
        let result = attempt_preview(
            input(Ok(&malformed)),
            &mut limiter,
            store,
            must_not_evaluate(),
        )
        .await;
        assert_eq!(result.reply, PREVIEW_UNAVAILABLE_REPLY);
        assert_eq!(result.metric, Some(metric_unavailable(Some(true))));
    }
}
