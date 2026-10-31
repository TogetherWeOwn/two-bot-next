//! Protected acceptance: unknown author flags must not expose this bot's posts.

#[allow(dead_code)]
mod common;

use common::{fresh_message_id, MockRest, ScriptedResponse};
use serde_json::{json, Value};
use two_bot_discord::{ActionExecutor, DiscordError};

const CHANNEL: &str = "4444";
const OWN_USER: &str = "777";
const REASON: &str = "purge own-message acceptance";

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("purge-own-message-fixture".to_owned(), Some(mock.origin()))
        .expect("executor uses the offline double")
}

async fn history_double(rows: Value, identity_status: u16) -> MockRest {
    // Route-aware replies keep an omitted identity read from consuming the
    // identity fixture as a bogus deletion response and obscuring the defect.
    MockRest::with_responder(move |request| {
        let path = request.path.split('?').next().expect("request path");
        if request.method == "GET" && path.ends_with("/channels/4444/messages") {
            ScriptedResponse::json(200, rows.clone())
        } else if request.method == "GET" && path.ends_with("/users/@me") {
            ScriptedResponse::json(identity_status, json!({"id": OWN_USER, "bot": true}))
        } else if (request.method == "POST"
            && path.ends_with("/channels/4444/messages/bulk-delete"))
            || (request.method == "DELETE" && path.contains("/channels/4444/messages/"))
        {
            ScriptedResponse::status(204)
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await
}

async fn assert_unknown_flag_protects_own_message(flag: Option<Value>) {
    let (own, other_unknown, human, pinned) = (
        fresh_message_id(1),
        fresh_message_id(2),
        fresh_message_id(3),
        fresh_message_id(4),
    );
    let mut own_author = json!({"id": OWN_USER});
    let mut other_author = json!({"id": "888"});
    if let Some(value) = flag {
        own_author["bot"] = value.clone();
        other_author["bot"] = value;
    }
    let mock = history_double(
        json!([
            {"id": own, "pinned": false, "author": own_author},
            {"id": other_unknown, "pinned": false, "author": other_author},
            {"id": human, "pinned": false, "author": {"id": "10", "bot": false}},
            {"id": pinned, "pinned": true, "author": {"id": "11", "bot": false}}
        ]),
        200,
    )
    .await;
    let result = executor_for(&mock).purge(CHANNEL, 4, REASON).await;
    let report = result.expect("history with an unknown bot flag is usable");
    assert_eq!(
        report.affected, 2,
        "own and pinned posts survive; another unknown author's post is eligible"
    );
    assert_eq!(report.stop_reason, None);
    let requests = mock.requests();
    assert_eq!(requests.len(), 3, "history, identity, one bulk delete");
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "GET");
    assert!(requests[1].path.ends_with("/users/@me"));
    assert_eq!(requests[2].method, "POST");
    assert!(requests[2].path.ends_with("/messages/bulk-delete"));
    let body: Value = serde_json::from_slice(&requests[2].body).expect("bulk-delete body");
    assert_eq!(body["messages"], json!([other_unknown, human]));
    mock.shutdown().await;
}

#[tokio::test]
async fn missing_bot_flag_protects_own_message() {
    assert_unknown_flag_protects_own_message(None).await;
}

#[tokio::test]
async fn null_bot_flag_protects_own_message() {
    assert_unknown_flag_protects_own_message(Some(Value::Null)).await;
}

#[tokio::test]
async fn string_true_bot_flag_protects_own_message() {
    assert_unknown_flag_protects_own_message(Some(json!("true"))).await;
}

#[tokio::test]
async fn string_false_bot_flag_is_not_an_explicit_non_bot() {
    assert_unknown_flag_protects_own_message(Some(json!("false"))).await;
}

#[tokio::test]
async fn numeric_bot_flag_protects_own_message() {
    assert_unknown_flag_protects_own_message(Some(json!(0))).await;
}

#[tokio::test]
async fn object_bot_flag_protects_own_message() {
    assert_unknown_flag_protects_own_message(Some(json!({}))).await;
}

#[tokio::test]
async fn array_bot_flag_protects_own_message() {
    assert_unknown_flag_protects_own_message(Some(json!([]))).await;
}

#[tokio::test]
async fn own_only_history_with_missing_flag_never_deletes() {
    let own = fresh_message_id(1);
    let mock = history_double(json!([{"id": own, "author": {"id": OWN_USER}}]), 200).await;
    let report = executor_for(&mock)
        .purge(CHANNEL, 1, REASON)
        .await
        .expect("protected history");
    assert_eq!(report.affected, 0);
    assert_eq!(report.stop_reason, None);
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "list and identity only");
    assert!(requests.iter().all(|request| request.method == "GET"));
    assert!(requests[1].path.ends_with("/users/@me"));
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_flag_with_unavailable_identity_never_deletes() {
    let own = fresh_message_id(1);
    let mock = history_double(json!([{"id": own, "author": {"id": OWN_USER}}]), 503).await;
    let result = executor_for(&mock).purge(CHANNEL, 1, REASON).await;
    assert!(
        matches!(result, Err(DiscordError::Unavailable(_))),
        "failed identity read must fail closed"
    );
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        2,
        "history then failed identity, no deletion"
    );
    assert!(requests.iter().all(|request| request.method == "GET"));
    assert!(requests[1].path.ends_with("/users/@me"));
    mock.shutdown().await;
}

#[tokio::test]
async fn explicitly_non_bot_history_never_reads_identity() {
    let (a, b, pinned) = (
        fresh_message_id(1),
        fresh_message_id(2),
        fresh_message_id(3),
    );
    let mock = history_double(
        json!([
            {"id": a, "pinned": false, "author": {"id": "10", "bot": false}},
            {"id": b, "pinned": false, "author": {"id": "11", "bot": false}},
            {"id": pinned, "pinned": true, "author": {"id": "12", "bot": false}}
        ]),
        503,
    )
    .await;
    let report = executor_for(&mock)
        .purge(CHANNEL, 3, REASON)
        .await
        .expect("non-bot history");
    assert_eq!(report.affected, 2);
    assert_eq!(report.stop_reason, None);
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        2,
        "list then bulk delete, no identity dependency"
    );
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "POST");
    assert!(requests[1].path.ends_with("/messages/bulk-delete"));
    assert!(!requests
        .iter()
        .any(|request| request.path.ends_with("/users/@me")));
    let body: Value = serde_json::from_slice(&requests[1].body).expect("bulk-delete body");
    assert_eq!(body["messages"], json!([a, b]));
    mock.shutdown().await;
}
