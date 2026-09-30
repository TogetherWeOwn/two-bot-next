//! TOG-10345: the `AuditMirror` adapter on `ActionExecutor` against the
//! scripted REST double. These tests pin the wire shape (enforced string
//! nonce, disabled mentions) and the `DiscordError` → `MirrorError`
//! classification the crash protocol depends on; the protocol itself is
//! exercised in the core service suite. Dev-only double, never a real token.

// `common` houses both the gateway double and the REST double; each test
// target uses one half (matches the `interaction_routing` precedent).
#[allow(dead_code)]
mod common;

use std::time::Duration;

use common::{MockRest, ScriptedResponse};
use two_bot_core::audit_mirror::{AuditMirror, MirrorError};
use two_bot_discord::ActionExecutor;

const CHANNEL: &str = "4444";
const GUILD: &str = "2222";
const BOT: &str = "9999";

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("s5-mirror-token".to_owned(), Some(mock.origin()))
        .expect("executor builds against the mock")
}

fn message_row(id: &str, author: &str, content: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "author": {"id": author}, "content": content})
}

#[tokio::test]
async fn post_mirror_sends_content_with_enforced_string_nonce() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            serde_json::json!({"id": "1234"}),
        )],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let outcome = exec
        .post_mirror(CHANNEL, "audit-event:42; · something happened", "oa_abc123")
        .await;
    assert_eq!(outcome, Ok("1234".to_owned()));
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1, "exactly one wire call");
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(
        reqs[0].path,
        format!("/api/v10/channels/{CHANNEL}/messages")
    );
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("post body is JSON");
    assert_eq!(body["content"], "audit-event:42; · something happened");
    assert_eq!(body["nonce"], "oa_abc123", "nonce stays a string");
    assert_eq!(body["enforce_nonce"], true);
    assert_eq!(
        body["allowed_mentions"],
        serde_json::json!({"parse": []}),
        "mentions are disabled on the wire"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn post_mirror_classifies_each_discord_failure() {
    // 4xx refusal is provably unsent; a pre-handler 429 is provably
    // unapplied; a 5xx is ambiguous. Timeout is covered by the delayed test.
    for (status, check) in [
        (403u16, "rejected"),
        (429, "rate-limited"),
        (500, "uncertain"),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::status(status)],
            ScriptedResponse::status(500),
        )
        .await;
        let exec = executor_for(&mock);
        let outcome = exec.post_mirror(CHANNEL, "x", "oa_nonce").await;
        match (check, outcome) {
            ("rejected", Err(MirrorError::Rejected(_)))
            | ("rate-limited", Err(MirrorError::RateLimited))
            | ("uncertain", Err(MirrorError::Uncertain(_))) => {}
            (_, other) => panic!("status {status} produced {other:?}"),
        }
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn post_mirror_timeout_is_uncertain_never_rejected() {
    // The 5 s executor abort: a post that may have landed must classify
    // Uncertain (reconcile-only), never Rejected (retryable).
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!({"id": "9"})).delayed(Duration::from_secs(6)),
    )
    .await;
    let exec = executor_for(&mock);
    let outcome = exec.post_mirror(CHANNEL, "x", "oa_nonce").await;
    assert!(
        matches!(outcome, Err(MirrorError::Uncertain(_))),
        "timeout maps to Uncertain, got {outcome:?}"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn channel_document_extracts_guild_and_everyone_overwrite() {
    let doc = serde_json::json!({
        "id": CHANNEL,
        "guild_id": GUILD,
        "name": "audit-log",
        "permission_overwrites": [
            {"id": "7777", "type": 1, "allow": "3072", "deny": "0"},
            {"id": GUILD, "type": 0, "allow": "0", "deny": "1024"}
        ]
    });
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, doc)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let channel = exec
        .channel_document(CHANNEL)
        .await
        .expect("document parses");
    assert_eq!(channel.guild_id, GUILD);
    let everyone = channel.everyone.expect("@everyone row is found");
    assert_eq!(everyone.allow, "0");
    assert_eq!(everyone.deny, "1024");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].path, format!("/api/v10/channels/{CHANNEL}"));
    mock.shutdown().await;
}

#[tokio::test]
async fn channel_document_missing_guild_and_overwrites_degrades_not_refuses() {
    // No guild_id → "" (the core fence classifies WrongGuild); no overwrite
    // array → None (the fence classifies PublicChannel). Neither is a wire
    // error.
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            serde_json::json!({"id": CHANNEL, "name": "dm-ish"}),
        )],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let channel = exec
        .channel_document(CHANNEL)
        .await
        .expect("sparse document parses");
    assert_eq!(channel.guild_id, "");
    assert!(channel.everyone.is_none());
    mock.shutdown().await;
}

#[tokio::test]
async fn channel_document_refuses_malformed_rows_and_errors() {
    // A malformed overwrite row must refuse (never silently "absent"); a
    // 404 is a refusal, not `Ok(None)` — the audit path needs `Rejected`,
    // which is exactly why `get_json`'s collapsing is not reused.
    for body in [
        serde_json::json!({"id": CHANNEL, "guild_id": GUILD, "permission_overwrites": "nope"}),
        serde_json::json!({"id": CHANNEL, "guild_id": GUILD, "permission_overwrites": [42]}),
        serde_json::json!([1, 2, 3]),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(200, body)],
            ScriptedResponse::status(500),
        )
        .await;
        let exec = executor_for(&mock);
        let outcome = exec.channel_document(CHANNEL).await;
        assert!(
            matches!(outcome, Err(MirrorError::Rejected(_))),
            "malformed document produced {outcome:?}"
        );
        mock.shutdown().await;
    }
    let mock = MockRest::start(
        vec![ScriptedResponse::status(404)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let outcome = exec.channel_document(CHANNEL).await;
    assert!(
        matches!(outcome, Err(MirrorError::Rejected(_))),
        "404 produced {outcome:?}"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn channel_history_pages_with_before_and_limit() {
    let page = serde_json::json!([
        message_row("300", BOT, "audit-event:7; · x"),
        message_row("250", "5555", "not ours"),
    ]);
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, page)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let rows = exec
        .channel_history(CHANNEL, Some("400"), 50)
        .await
        .expect("page parses");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "300");
    assert_eq!(rows[0].author_id, BOT);
    assert_eq!(rows[1].author_id, "5555");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "GET");
    assert!(
        reqs[0]
            .path
            .starts_with(&format!("/api/v10/channels/{CHANNEL}/messages?")),
        "history path: {}",
        reqs[0].path
    );
    assert!(reqs[0].path.contains("before=400"), "path {}", reqs[0].path);
    assert!(reqs[0].path.contains("limit=50"), "path {}", reqs[0].path);
    mock.shutdown().await;
}

#[tokio::test]
async fn channel_history_skips_malformed_rows_and_refuses_non_arrays() {
    // Reconcile only ever needs one readable marked row: a poisoned element
    // is skipped, while a non-array body is an unreadable page (Rejected).
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            serde_json::json!([
                {"id": "300"},
                message_row("250", BOT, "audit-event:7; · x"),
                "garbage",
            ]),
        )],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let rows = exec
        .channel_history(CHANNEL, None, 100)
        .await
        .expect("page parses");
    assert_eq!(rows.len(), 1, "only the readable row survives");
    assert_eq!(rows[0].id, "250");
    let reqs = mock.requests();
    assert!(!reqs[0].path.contains("before="), "path {}", reqs[0].path);
    mock.shutdown().await;

    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            serde_json::json!({"not": "an array"}),
        )],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor_for(&mock);
    let outcome = exec.channel_history(CHANNEL, None, 100).await;
    assert!(
        matches!(outcome, Err(MirrorError::Rejected(_))),
        "non-array body produced {outcome:?}"
    );
    mock.shutdown().await;
}
