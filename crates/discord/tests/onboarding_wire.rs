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
                &allowed_mentions(MentionPolicy::Member(mention_user_id))
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
    exec.post_channel_message(&channel_id, &content, &[], &allowed_mentions(mentions))
        .await
        .unwrap();
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 4, "one channel post per effect");
    for (index, req) in reqs.iter().enumerate() {
        assert_eq!(req.method, "POST");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
        assert_eq!(body["allowed_mentions"]["roles"], serde_json::json!([]));
        if index < 3 {
            assert_eq!(body["allowed_mentions"]["users"], serde_json::json!(["44"]));
        } else {
            assert_eq!(body["allowed_mentions"]["users"], serde_json::json!([]));
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
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
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
    let before = exec.requests();
    assert!(matches!(
        exec.edit_interaction_response(11, "mock-callback", &"😀".repeat(1001))
            .await,
        Err(DiscordError::Rejected(_))
    ));
    assert_eq!(before, exec.requests(), "reject oversized edit before wire");
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
        exec.post_channel_message(
            "12",
            "welcome",
            &[],
            &allowed_mentions(MentionPolicy::Member(44))
        )
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
        .post_channel_message(
            "12",
            &"😀".repeat(1001),
            &[],
            &allowed_mentions(MentionPolicy::None)
        )
        .await
        .is_err());
    assert_eq!(before, exec.requests(), "reject before wire");
}
