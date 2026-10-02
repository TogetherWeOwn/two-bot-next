//! Hermetic onboarding transport proof against the shared scripted REST double.
#![allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use two_bot_core::onboarding::*;
use two_bot_discord::onboarding_messages::*;
use two_bot_discord::{ActionExecutor, DiscordError};

fn executor(mock: &MockRest) -> ActionExecutor {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    ActionExecutor::with_proxy("onboarding-mock-token".into(), Some(mock.origin())).unwrap()
}

#[tokio::test]
async fn onboarding_permission_evidence_distinguishes_unavailable_from_denied() {
    use serde_json::json;
    use two_bot_discord::onboarding_permissions::{AccessUnavailable, MemberAccess};

    let mock = MockRest::start(vec![], ScriptedResponse::status(503)).await;
    assert!(executor(&mock)
        .get_json_checked("/guilds/22")
        .await
        .is_err());
    assert!(matches!(
        MemberAccess::load(&executor(&mock), 22, 44).await,
        Err(AccessUnavailable)
    ));
    mock.shutdown().await;

    let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    assert!(MemberAccess::load(&executor(&mock), 22, 44)
        .await
        .unwrap()
        .is_none());
    mock.shutdown().await;

    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"22", "owner_id":"99"})),
            ScriptedResponse::json(200, json!([{"id":"22", "permissions":"3072"}])),
            ScriptedResponse::json(200, json!({"user":{"id":"44"}, "roles":[]})),
            ScriptedResponse::status(404),
        ],
        ScriptedResponse::status(503),
    )
    .await;
    let access = MemberAccess::load(&executor(&mock), 22, 44)
        .await
        .unwrap()
        .unwrap();
    assert!(!access.permits(&executor(&mock), "12", true).await.unwrap());
    assert_eq!(
        access.permits(&executor(&mock), "12", true).await,
        Err(AccessUnavailable)
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn shared_executor_renders_each_mode_and_goodbye_without_extra_messages() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(201, serde_json::json!({"id":"99"})),
    )
    .await;
    let exec = executor(&mock);
    let picks = build_session_picks("10", "11");
    for mode in [
        OnboardingMode::Legacy,
        OnboardingMode::Session,
        OnboardingMode::Anchor,
    ] {
        let WelcomeEffect::Post {
            channel_id,
            content,
            mention_user_id,
            picker,
        } = adjudicate_welcome(
            mode,
            44,
            Some("12"),
            "13",
            1_790_780_400,
            SUNDAY_SQUAD,
            false,
        )
        else {
            panic!("welcome");
        };
        let components = picker_components(picker, &[GAME_PICKS[0].role_id], &picks);
        assert_eq!(
            exec.post_channel_message(
                &channel_id,
                &content,
                &components,
                MentionPolicy::Member(mention_user_id)
            )
            .await
            .unwrap(),
            "99"
        );
    }
    let Some(GoodbyeEffect::Post {
        channel_id,
        content,
        mentions,
    }) = adjudicate_goodbye(
        OnboardingMode::Session,
        "@everyone <@44> <@&45>",
        Some(2),
        Some("14"),
        false,
    )
    else {
        panic!("goodbye");
    };
    exec.post_channel_message(&channel_id, &content, &[], mentions)
        .await
        .unwrap();
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 4, "one channel post per effect");
    for (index, req) in reqs.iter().enumerate() {
        assert_eq!(req.method, "POST");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
        assert_eq!(body["allowed_mentions"]["roles"], serde_json::json!([]));
        assert_eq!(body["allowed_mentions"]["replied_user"], false);
        if index < 3 {
            assert_eq!(body["allowed_mentions"]["users"], serde_json::json!(["44"]));
        } else {
            assert_eq!(body["allowed_mentions"]["users"], serde_json::json!([]));
            assert_eq!(
                body["content"],
                two_bot_core::message_safety::content(&content)
            );
            assert!(!body["content"].as_str().unwrap().contains("@everyone"));
        }
        if index >= 2 {
            assert!(
                body.get("components").is_none(),
                "anchor/goodbye attach nothing"
            );
            assert!(body.get("embeds").is_none());
        } else {
            assert_eq!(
                body["components"][0]["components"][0]["custom_id"],
                if index == 0 {
                    GAME_SELECT_ID
                } else {
                    SESSION_SELECT_ID
                }
            );
        }
    }
}

#[tokio::test]
async fn shared_executor_defer_role_delta_and_edit_are_not_broadcasts() {
    // Callback and role writes answer 204; the edit returns the edited message.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::status(204),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, serde_json::json!({"id":"98"})),
            ScriptedResponse::json(200, serde_json::json!({"id":"98"})),
            ScriptedResponse::json(200, serde_json::json!({"id":"98"})),
        ],
        ScriptedResponse::status(204),
    )
    .await;
    let exec = executor(&mock);
    exec.answer_interaction(33, "mock-callback", &defer_ephemeral())
        .await
        .unwrap();
    exec.set_member_role("22", "44", "55", true, "member selected games")
        .await
        .unwrap();
    exec.set_member_role("22", "44", "66", false, "member deselected games")
        .await
        .unwrap();
    exec.edit_interaction_response(11, "mock-callback", "Done")
        .await
        .unwrap();
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 4);
    assert_eq!(
        reqs[0].path,
        "/api/v10/interactions/33/mock-callback/callback"
    );
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["data"]["flags"], 64);
    assert_eq!(reqs[1].method, "PUT");
    assert_eq!(reqs[1].path, "/api/v10/guilds/22/members/44/roles/55");
    assert_eq!(reqs[2].method, "DELETE");
    assert_eq!(reqs[2].path, "/api/v10/guilds/22/members/44/roles/66");
    assert_eq!(reqs[3].method, "PATCH");
    assert_eq!(
        reqs[3].path,
        "/api/v10/webhooks/11/mock-callback/messages/@original"
    );
    assert!(!reqs.iter().any(|r| r.path.contains("/channels/")));
    assert!(
        reqs[3].header("authorization").is_none(),
        "webhook edits use only their callback token"
    );
    let body: serde_json::Value = serde_json::from_slice(&reqs[3].body).unwrap();
    assert_eq!(body["allowed_mentions"]["users"], serde_json::json!([]));
    assert_eq!(body["allowed_mentions"]["roles"], serde_json::json!([]));
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
    exec.edit_interaction_response(11, "mock-callback", &"😀".repeat(1001))
        .await
        .unwrap();
    exec.edit_interaction_response(11, "mock-callback", "")
        .await
        .unwrap();
    let edits = mock.requests();
    assert_eq!(edits.len(), 6);
    for (index, expected) in [(4, "😀".repeat(1000)), (5, String::new())] {
        assert_eq!(edits[index].method, "PATCH");
        assert!(edits[index].header("authorization").is_none());
        let body: serde_json::Value = serde_json::from_slice(&edits[index].body).unwrap();
        assert_eq!(body["content"], expected);
        assert_eq!(
            body["allowed_mentions"],
            serde_json::json!({"parse":[], "roles":[], "users":[], "replied_user":false})
        );
    }
    assert!(
        reqs[2]
            .received_at
            .duration_since(reqs[1].received_at)
            .as_millis()
            >= 100
    );
}

#[tokio::test]
async fn shared_executor_failed_post_or_role_write_is_not_retried() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    let exec = executor(&mock);
    assert!(matches!(
        exec.post_channel_message("12", "welcome", &[], MentionPolicy::Member(44))
            .await,
        Err(DiscordError::Rejected(_))
    ));
    assert!(matches!(
        exec.set_member_role("22", "44", "55", true, "member selected games")
            .await,
        Err(DiscordError::Rejected(_))
    ));
    assert_eq!(mock.requests().len(), 2);
    let before = exec.requests();
    assert!(exec
        .post_channel_message("12", &"😀".repeat(1001), &[], MentionPolicy::None)
        .await
        .is_err());
    assert_eq!(before, exec.requests(), "reject before wire");
}

#[tokio::test]
async fn shared_component_posts_bound_text_and_allow_only_the_welcome_recipient() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(201, serde_json::json!({"id":"99"})),
    )
    .await;
    let exec = executor(&mock);
    let components = picker_components(Some(PickerKind::Games), &[], &[]);
    let injected = "@everyone @here @\u{200b}everyone <@44> <@55> <@&77>";
    for policy in [MentionPolicy::None, MentionPolicy::Member(44)] {
        exec.post_channel_message("12", injected, &components, policy)
            .await
            .unwrap();
    }
    let boundary = format!("{}@everyone!", "😀".repeat(995));
    assert_eq!(boundary.encode_utf16().count(), 2000);
    exec.post_channel_message("12", &boundary, &[], MentionPolicy::Member(44))
        .await
        .unwrap();
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    for (index, req) in reqs.iter().enumerate() {
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        let content = body["content"].as_str().unwrap();
        assert_eq!(
            content,
            two_bot_core::message_safety::content(if index < 2 { injected } else { &boundary })
        );
        assert!(!content.contains("@everyone"));
        assert!(!content.contains("@here"));
        assert!(content.encode_utf16().count() <= 2000);
        assert_eq!(
            body["allowed_mentions"],
            serde_json::json!({
                "parse":[], "roles":[], "replied_user":false,
                "users": if index == 0 { vec![] } else { vec!["44"] },
            })
        );
        if index < 2 {
            assert_eq!(
                body["components"],
                serde_json::to_value(&components).unwrap()
            );
        } else {
            assert_eq!(content.encode_utf16().count(), 2000);
            assert!(content.ends_with("@\u{200b}everyone"));
            assert!(body.get("components").is_none());
        }
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn shared_component_posts_reject_empty_payloads_but_keep_component_only_creates() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(201, serde_json::json!({"id":"99"})),
    )
    .await;
    let exec = executor(&mock);
    for content in ["", " \t\n", "\u{200b}"] {
        assert!(matches!(
            exec.post_channel_message("12", content, &[], MentionPolicy::None)
                .await,
            Err(DiscordError::Rejected(_))
        ));
    }
    assert!(matches!(
        exec.post_channel_message("12", "welcome", &[], MentionPolicy::Member(0))
            .await,
        Err(DiscordError::Rejected(_))
    ));
    assert_eq!(exec.requests(), 0, "invalid creates never reach the wire");
    assert!(mock.requests().is_empty());
    let components = picker_components(Some(PickerKind::Games), &[], &[]);
    exec.post_channel_message("12", "", &components, MentionPolicy::None)
        .await
        .unwrap();
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(body.get("content").is_none());
    assert_eq!(
        body["components"],
        serde_json::to_value(&components).unwrap()
    );
    assert_eq!(
        body["allowed_mentions"],
        serde_json::json!({"parse":[], "roles":[], "users":[], "replied_user":false})
    );
    mock.shutdown().await;
}
