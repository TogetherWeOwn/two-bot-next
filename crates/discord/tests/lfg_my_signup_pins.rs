//! Builder-owned pin for the My signup message-authenticity fence.
//! The protected acceptance file gives the bot user and the application the
//! same id, so it cannot tell which one the fence compares. Here they differ:
//! only a message authored by the bot USER is genuine; one authored by the
//! application id is refused without disclosing the saved row.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{lfg, lfg_store as store, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const MESSAGE: &str = "5000";
const BOT_USER: u64 = 1111;
const APPLICATION: u64 = 7777;
const MEMBER: u64 = 3333;
const POST: &str = "lfg-pin-fixture";
const LABEL: &str = "Pinned tank label";

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        announcements: true,
        scorecard: false,
        automations: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn press(id: u64, author: u64) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": APPLICATION.to_string(), "type": 3,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"user": {"id": MEMBER.to_string(), "username": "fixture-member", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0, "permissions": "0"},
        "token": format!("lfg-pin-{id}"), "version": 1, "entitlements": [],
        "authorizing_integration_owners": {"0": GUILD},
        "data": {"custom_id": format!("two:lfg-mine:{POST}"), "component_type": 2},
        "message": {
            "id": MESSAGE, "guild_id": GUILD, "channel_id": CHANNEL, "type": 0,
            "author": {"id": author.to_string(), "username": "fixture-bot", "discriminator": "0", "avatar": null, "bot": true},
            "content": "Fixture raid", "timestamp": "2026-10-20T10:00:00+00:00",
            "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false,
            "components": [{"type": 1, "components": [{"type": 2, "style": 2,
                "label": "My signup", "custom_id": format!("two:lfg-mine:{POST}")}]}]
        }
    }))
    .expect("complete synthetic interaction")
}

fn bodies(requests: &[RestRequest]) -> String {
    requests
        .iter()
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body)
                .map(|v| v.to_string())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn only_a_message_authored_by_the_bot_user_not_the_application_id_is_genuine() {
    let url =
        std::env::var("TWO_TEST_DATABASE_URL").expect("guarded disposable bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create guarded disposable fixture");
    let roles = lfg::spec_roles(
        POST,
        &lfg::parse_role_spec(&format!("tank:{LABEL}:9")).expect("valid fixture roles"),
    );
    let post = lfg::LfgPost {
        id: POST.into(),
        guild_id: GUILD.into(),
        channel_id: CHANNEL.into(),
        message_id: Some(MESSAGE.into()),
        title: "Pin raid".into(),
        starts_at: "2099-09-11T20:00:00Z".into(),
        status: lfg::LfgStatus::Open,
        created_by: MEMBER.to_string(),
        created_at: "2026-10-20T10:00:00Z".into(),
        closed_at: None,
    };
    store::put_lfg(db.pool(), &post, &roles, true)
        .await
        .expect("seed real post");
    store::signup_lfg(
        db.pool(),
        GUILD,
        POST,
        "tank",
        &MEMBER.to_string(),
        "2026-10-20T10:11:12Z",
    )
    .await
    .expect("seed signup");
    let mock = MockRest::with_responder(|request| {
        if request.path.ends_with("/callback") {
            ScriptedResponse::status(204)
        } else if request.method == "PATCH" && request.path.ends_with("/messages/@original") {
            ScriptedResponse::json(200, json!({"id": "6000"}))
        } else {
            ScriptedResponse::status(404)
        }
    })
    .await;
    let rt = InteractionRuntime::new(
        gates(),
        db.pool().clone(),
        ActionExecutor::with_proxy("lfg-pin-token".into(), Some(mock.origin()))
            .expect("fixture executor"),
        BOT_USER,
        two_bot_core::ClassifierConfig::default(),
    );
    rt.set_application_id(APPLICATION);

    let forged_offset = mock.requests().len();
    assert!(rt
        .handle(&press(9001, APPLICATION))
        .await
        .expect("forged press completes"));
    let forged = bodies(&mock.requests()[forged_offset..]);
    let genuine_offset = mock.requests().len();
    assert!(rt
        .handle(&press(9002, BOT_USER))
        .await
        .expect("genuine press completes"));
    let genuine = bodies(&mock.requests()[genuine_offset..]);

    drop(rt);
    mock.shutdown().await;
    db.close().await.expect("disposable database teardown");

    assert!(
        !forged.contains(LABEL),
        "a message authored by the application id must not disclose the signup: {forged}"
    );
    assert!(
        genuine.contains(LABEL),
        "a message authored by the bot user must show the saved role: {genuine}"
    );
}
