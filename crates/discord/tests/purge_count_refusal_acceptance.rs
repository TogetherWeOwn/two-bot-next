//! Protected acceptance: invalid purge counts refuse before any Discord I/O.

#[allow(dead_code)]
mod common;

use common::{fresh_message_id, MockRest, ScriptedResponse};
use two_bot_core::{ActionOutcome, ModerationAction, ModerationExecution};
use two_bot_discord::{ActionExecutor, DiscordError};

const CHANNEL: &str = "4444";
const REASON: &str = "purge count acceptance";

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("purge-count-fixture".to_owned(), Some(mock.origin()))
        .expect("executor uses the offline double")
}

fn execution(count: Option<u64>) -> ModerationExecution {
    ModerationExecution {
        guild_id: "2222".to_owned(),
        channel_id: Some(CHANNEL.to_owned()),
        action: ModerationAction::Purge,
        target_user_id: None,
        reason: REASON.to_owned(),
        duration_seconds: None,
        count,
        seconds: None,
    }
}

fn planned_outcome() -> ActionOutcome {
    ActionOutcome::Purged {
        channel_id: CHANNEL.to_owned(),
        affected: 0,
    }
}

async fn assert_execution_refuses(count: Option<u64>) {
    // A healthy empty history distinguishes local refusal from a remote error.
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, serde_json::json!([]))],
        ScriptedResponse::status(500),
    )
    .await;
    let result = executor_for(&mock)
        .execute_outcome(&execution(count), &planned_outcome())
        .await;
    let requests = mock.requests();
    assert!(
        requests.is_empty(),
        "count {count:?} must refuse before list, identity or delete I/O; sent {} requests",
        requests.len()
    );
    assert!(
        matches!(result, Err(DiscordError::Rejected(_))),
        "count {count:?} must be a local refusal"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn missing_execution_count_refuses_before_any_io() {
    assert_execution_refuses(None).await;
}

#[tokio::test]
async fn zero_execution_count_refuses_before_any_io() {
    assert_execution_refuses(Some(0)).await;
}

#[tokio::test]
async fn count_101_execution_refuses_instead_of_clamping() {
    assert_execution_refuses(Some(101)).await;
}

#[tokio::test]
async fn maximum_execution_count_refuses_before_narrowing() {
    assert_execution_refuses(Some(u64::MAX)).await;
}

async fn assert_direct_refuses(count: u64) {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, serde_json::json!([]))],
        ScriptedResponse::status(500),
    )
    .await;
    let result = executor_for(&mock).purge(CHANNEL, count, REASON).await;
    assert!(
        mock.requests().is_empty(),
        "direct purge({count}) must refuse before any I/O"
    );
    assert!(matches!(result, Err(DiscordError::Rejected(_))));
    mock.shutdown().await;
}

#[tokio::test]
async fn direct_zero_count_refuses_before_any_io() {
    assert_direct_refuses(0).await;
}

#[tokio::test]
async fn direct_count_101_refuses_instead_of_clamping() {
    assert_direct_refuses(101).await;
}

async fn assert_valid_boundary(count: u64, through_outcome: bool) {
    let ids: Vec<_> = (1..=count).map(fresh_message_id).collect();
    let rows: Vec<_> = ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": id, "pinned": false, "author": {"id": "10", "bot": false}
            })
        })
        .collect();
    let lone_id = ids[0].clone();
    let mock = MockRest::with_responder(move |request| {
        let path = request.path.split('?').next().expect("request path");
        if request.method == "GET" && path.ends_with("/channels/4444/messages") {
            ScriptedResponse::json(200, serde_json::json!(rows))
        } else if (count == 1
            && request.method == "DELETE"
            && path.ends_with(&format!("/channels/4444/messages/{lone_id}")))
            || (count == 100
                && request.method == "POST"
                && path.ends_with("/channels/4444/messages/bulk-delete"))
        {
            ScriptedResponse::status(204)
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await;
    let executor = executor_for(&mock);
    if through_outcome {
        assert_eq!(
            executor
                .execute_outcome(&execution(Some(count)), &planned_outcome())
                .await
                .expect("valid count executes"),
            ActionOutcome::Purged {
                channel_id: CHANNEL.to_owned(),
                affected: count as usize,
            }
        );
    } else {
        let report = executor
            .purge(CHANNEL, count, REASON)
            .await
            .expect("valid purge");
        assert_eq!(report.affected, count);
        assert_eq!(report.stop_reason, None);
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "history then deletion; no identity read");
    assert_eq!(requests[0].method, "GET");
    let query = requests[0].path.split_once('?').expect("list query").1;
    assert!(query
        .split('&')
        .any(|part| part == format!("limit={count}")));
    if count == 1 {
        assert_eq!(requests[1].method, "DELETE");
        assert!(requests[1].path.ends_with(&format!("/messages/{}", ids[0])));
    } else {
        assert_eq!(requests[1].method, "POST");
        let body: serde_json::Value =
            serde_json::from_slice(&requests[1].body).expect("bulk-delete body");
        assert_eq!(body["messages"], serde_json::json!(ids));
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn execution_count_one_lists_and_deletes_one_message() {
    assert_valid_boundary(1, true).await;
}

#[tokio::test]
async fn execution_count_100_lists_and_deletes_all_100_messages() {
    assert_valid_boundary(100, true).await;
}

#[tokio::test]
async fn direct_count_one_lists_and_deletes_one_message() {
    assert_valid_boundary(1, false).await;
}

#[tokio::test]
async fn direct_count_100_lists_and_deletes_all_100_messages() {
    assert_valid_boundary(100, false).await;
}
