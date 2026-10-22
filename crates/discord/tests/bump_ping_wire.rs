//! Hermetic bump-ping transport proof against the shared scripted REST double.
#![allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use two_bot_discord::{ActionExecutor, DiscordError};

fn executor(mock: &MockRest) -> ActionExecutor {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    ActionExecutor::with_proxy("bump-mock-token".into(), Some(mock.origin())).unwrap()
}

#[tokio::test]
async fn role_ping_posts_pill_with_role_allowlist_only() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(201, serde_json::json!({"id":"99"})),
    )
    .await;
    let id = executor(&mock)
        .post_message_with_role_mention(
            "4444",
            "<@&8888> The bump cooldown is over — the server can be bumped again!",
            "8888",
        )
        .await
        .unwrap();
    assert_eq!(id, "99");

    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "one ping, one POST");
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/channels/4444/messages"));
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let content = body["content"].as_str().unwrap();
    assert!(content.contains("<@&8888>"), "pill survives: {content:?}");
    assert_eq!(
        body["allowed_mentions"]["roles"],
        serde_json::json!(["8888"])
    );
    assert!(body["allowed_mentions"]["users"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(body["allowed_mentions"]["parse"]
        .as_array()
        .unwrap()
        .is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn role_ping_neutralizes_mass_mentions_and_rejects_bad_role() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(201, serde_json::json!({"id":"99"})),
    )
    .await;
    let exec = executor(&mock);
    exec.post_message_with_role_mention("4444", "@everyone <@&8888> bump now", "8888")
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&mock.requests()[0].body).unwrap();
    let content = body["content"].as_str().unwrap();
    assert!(
        !content.contains("@everyone"),
        "mass mention neutralized: {content:?}"
    );
    assert!(!content.contains("@here"));
    assert!(
        content.contains("<@&8888>"),
        "role pill survives: {content:?}"
    );

    for bad_role in ["0", "nope", ""] {
        let before = mock.requests().len();
        let err = exec
            .post_message_with_role_mention("4444", "<@&8888> x", bad_role)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DiscordError::Rejected(_)),
            "bad role {bad_role:?} rejects, got {err:?}"
        );
        assert_eq!(mock.requests().len(), before, "zero wire calls on reject");
    }
    mock.shutdown().await;
}
