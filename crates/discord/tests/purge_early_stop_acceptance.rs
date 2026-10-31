//! Acceptance: a purge that stops early says why, in the executor result.
//!
//! The single-delete phase stops at the first refusal that proves only the
//! failing call had no effect, once at least one message is already gone.
//! The executor result must report both the count deleted so far and a
//! fixed-vocabulary stop reason (`refused` for a 403, `rate_limited` for a
//! 429); a purge that ran to completion reports no stop reason. A refusal
//! on the very first delete still errors: nothing was deleted, so there is
//! no early stop to report.
//!
//! Contract pinned here: `ActionExecutor::purge` returns
//! `PurgeReport { affected, stop_reason }` from `two_bot_core`.
//!
//! Dev-only: never ships in the release binary.

#[allow(dead_code)]
mod common;

use common::{aged_message_id, MockRest, ScriptedResponse};
use two_bot_core::{PurgeReport, PurgeStopReason};
use two_bot_discord::{ActionExecutor, DiscordError};

const CHANNEL: &str = "4444";
const REASON: &str = "early stop acceptance";

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("early-stop-token".to_owned(), Some(mock.origin()))
        .expect("executor builds against the mock")
}

fn history(ids: &[String]) -> ScriptedResponse {
    let rows = ids
        .iter()
        .map(|id| serde_json::json!({"id": id, "author": {"bot": false}}))
        .collect();
    ScriptedResponse::json(200, serde_json::Value::Array(rows))
}

/// Messages older than 14 days are deleted one by one, which is the loop
/// that can stop part-way. One delete succeeds, the next is answered with
/// `refusal`, and the third message must never be attempted.
async fn stopped_report(refusal: ScriptedResponse) -> PurgeReport {
    let ids = [aged_message_id(1), aged_message_id(2), aged_message_id(3)];
    let mock = MockRest::start(
        vec![history(&ids), ScriptedResponse::status(204), refusal],
        ScriptedResponse::status(500),
    )
    .await;
    let report = executor_for(&mock)
        .purge(CHANNEL, 3, REASON)
        .await
        .expect("a purge that deleted a message reports an early stop, not an error");
    assert_eq!(
        mock.requests().len(),
        3,
        "list, one successful delete, the refused delete; nothing after the stop"
    );
    mock.shutdown().await;
    report
}

#[tokio::test]
async fn purge_stopped_by_a_refusal_reports_the_count_and_refused_reason() {
    let report = stopped_report(ScriptedResponse::status(403)).await;
    assert_eq!(
        report,
        PurgeReport {
            affected: 1,
            stop_reason: Some(PurgeStopReason::Refused),
        }
    );
    assert_eq!(
        report.stop_reason.map(|reason| reason.name()),
        Some("refused")
    );
}

#[tokio::test]
async fn purge_stopped_by_a_rate_limit_reports_the_count_and_rate_limited_reason() {
    let report = stopped_report(ScriptedResponse::rate_limited(0.0, "0")).await;
    assert_eq!(
        report,
        PurgeReport {
            affected: 1,
            stop_reason: Some(PurgeStopReason::RateLimited),
        }
    );
    assert_eq!(
        report.stop_reason.map(|reason| reason.name()),
        Some("rate_limited")
    );
}

#[tokio::test]
async fn full_purge_reports_the_count_and_no_stop_reason() {
    let ids = [aged_message_id(1), aged_message_id(2), aged_message_id(3)];
    let mock = MockRest::start(vec![history(&ids)], ScriptedResponse::status(204)).await;
    let report = executor_for(&mock)
        .purge(CHANNEL, 3, REASON)
        .await
        .expect("full purge succeeds");
    assert_eq!(
        report,
        PurgeReport {
            affected: 3,
            stop_reason: None,
        }
    );
    assert_eq!(mock.requests().len(), 4, "list plus one delete per message");
    mock.shutdown().await;
}

#[tokio::test]
async fn first_delete_refusal_still_errors_with_nothing_deleted() {
    let ids = [aged_message_id(1), aged_message_id(2)];
    let mock = MockRest::start(
        vec![history(&ids), ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let result = executor_for(&mock).purge(CHANNEL, 2, REASON).await;
    assert!(
        matches!(result, Err(DiscordError::Rejected(_))),
        "nothing deleted yet, so the refusal is an error, not an early stop"
    );
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}
