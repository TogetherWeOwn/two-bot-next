//! Ticket-specific wire acceptance through the shared S4 mock Discord double.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use twilight_model::guild::Permissions;
use two_bot_discord::{
    executor::{ChannelPresence, TicketChannelRequest, TicketMessage},
    ActionExecutor,
};

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap()
}

fn channel() -> TicketChannelRequest<'static> {
    TicketChannelRequest {
        guild_id: "100",
        category_id: "200",
        staff_role_id: "300",
        bot_id: "400",
        opener_id: "500",
        username: "Unsafe Username!!!",
        reservation_id: "reservation",
    }
}

#[tokio::test]
async fn ticket_create_has_exact_metadata_and_least_privilege_overwrites() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(201, json!({"id":"600"}))).await;
    assert_eq!(
        executor(&mock)
            .create_ticket_channel(&channel())
            .await
            .unwrap(),
        "600"
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/guilds/100/channels"));
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["name"], "ticket-unsafe-username");
    assert_eq!(body["type"], 0);
    assert_eq!(body["parent_id"], "200");
    assert_eq!(body["topic"], "two-ticket:reservation");
    let common = (Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY)
        .bits();
    assert_eq!(
        body["permission_overwrites"],
        json!([
            {"id":"100","type":0,"allow":"0","deny":Permissions::VIEW_CHANNEL.bits().to_string()},
            {"id":"400","type":1,"allow":(common | Permissions::MANAGE_CHANNELS.bits()).to_string(),"deny":"0"},
            {"id":"500","type":1,"allow":common.to_string(),"deny":"0"},
            {"id":"300","type":0,"allow":common.to_string(),"deny":"0"},
        ])
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn uncertain_creates_are_never_automatically_retried() {
    for response in [
        ScriptedResponse::status(500),
        ScriptedResponse::rate_limited(0.001, "0"),
        ScriptedResponse::json(200, json!({"id":"not-a-channel"})),
    ] {
        let mock = MockRest::start(vec![], response).await;
        let error = executor(&mock)
            .create_ticket_channel(&channel())
            .await
            .unwrap_err();
        assert!(!error.is_safe_pre_mutation());
        assert_eq!(mock.requests().len(), 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn everyone_cannot_be_configured_as_staff() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let mut input = channel();
    input.staff_role_id = input.guild_id;
    assert!(executor(&mock)
        .create_ticket_channel(&input)
        .await
        .unwrap_err()
        .is_safe_pre_mutation());
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn only_numeric_unknown_channel_is_absence_evidence() {
    for (status, body, absent) in [
        (404, json!({"code":10003,"message":"anything"}), true),
        (
            404,
            json!({"code":50013,"message":"Unknown Channel"}),
            false,
        ),
        (404, json!({"message":"Unknown Channel"}), false),
        (404, json!({"code":"10003"}), false),
        (403, json!({"code":10003}), false),
        (500, json!({"code":10003}), false),
    ] {
        let mock = MockRest::start(vec![], ScriptedResponse::json(status, body)).await;
        let rest = executor(&mock);
        let read = rest.fetch_ticket_channel("600").await;
        assert_eq!(matches!(read, Ok(ChannelPresence::Absent)), absent);
        let delete = rest.delete_ticket_channel("600").await;
        assert_eq!(delete.is_ok(), absent);
        assert_eq!(mock.requests().len(), 2);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn successful_delete_proves_absence() {
    for response in [
        ScriptedResponse::status(204),
        ScriptedResponse::json(200, json!({"id":"600"})),
    ] {
        let mock = MockRest::start(vec![], response).await;
        assert!(executor(&mock).delete_ticket_channel("600").await.is_ok());
        assert_eq!(mock.requests().len(), 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn opener_freeze_and_restore_preserve_unmodeled_permission_bits() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let rest = executor(&mock);
    let writes = Permissions::SEND_MESSAGES.bits();
    let allow = (1 << 48) | writes;
    let deny = 1 << 47;
    rest.set_ticket_opener_writes("600", "500", allow, deny, false)
        .await
        .unwrap();
    rest.set_ticket_opener_writes("600", "500", allow & !writes, deny | writes, true)
        .await
        .unwrap();
    let requests = mock.requests();
    let frozen: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let restored: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(requests[0].method, "PUT");
    assert!(requests[0].path.ends_with("/channels/600/permissions/500"));
    assert_eq!(
        frozen,
        json!({"allow":(allow & !writes).to_string(), "deny":(deny | writes).to_string(), "type":1})
    );
    assert_eq!(
        restored,
        json!({"allow":allow.to_string(), "deny":deny.to_string(), "type":1})
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn fixed_panel_and_controls_enforce_styles_and_controlled_mentions() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"700"}))).await;
    let rest = executor(&mock);
    rest.post_ticket_message("600", TicketMessage::Panel)
        .await
        .unwrap();
    rest.post_ticket_message("600", TicketMessage::Controls { opener_id: "500" })
        .await
        .unwrap();
    let requests = mock.requests();
    let panel: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let controls: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(panel["content"], two_bot_core::tickets::PANEL_TEXT);
    assert_eq!(
        panel["components"][0]["components"][0],
        json!({"type":2,"style":1,"label":"Open a ticket","custom_id":"two:tickets:open"})
    );
    assert_eq!(
        panel["allowed_mentions"],
        json!({"parse":[],"roles":[],"users":[],"replied_user":false})
    );
    assert_eq!(
        controls["components"][0]["components"],
        json!([
            {"type":2,"style":2,"label":"Claim","custom_id":"two:tickets:claim"},
            {"type":2,"style":4,"label":"Close","custom_id":"two:tickets:close"},
        ])
    );
    assert_eq!(
        controls["allowed_mentions"],
        json!({"parse":[],"roles":[],"users":["500"],"replied_user":false})
    );
    assert!(controls["content"].as_str().unwrap().contains("90 days"));
    mock.shutdown().await;
}
