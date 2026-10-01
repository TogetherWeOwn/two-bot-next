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

#[tokio::test]
async fn resumed_identity_is_authenticated_bot_user_not_application() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!({"id":"400","bot":true}))],
        ScriptedResponse::json(200, json!({"id":"900"})),
    )
    .await;
    let rest = executor(&mock);
    assert_eq!(rest.current_bot_user_id().await.unwrap(), 400);
    assert_eq!(rest.current_application_id().await.unwrap(), 900);
    assert_eq!(mock.requests()[0].path, "/api/v10/users/@me");
    assert_eq!(mock.requests()[1].path, "/api/v10/applications/@me");
    assert_eq!(
        mock.requests()[0].header("authorization"),
        Some("Bot ticket-test-token")
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn resumed_identity_refuses_invalid_evidence_without_retry_or_mutation() {
    let mut responses = vec![
        ScriptedResponse::status(403),
        ScriptedResponse::status(429),
        ScriptedResponse::status(500),
        ScriptedResponse::status(200),
        ScriptedResponse::json(200, json!({"id":"900"})),
        ScriptedResponse::json(200, json!({"id":"400","bot":false})),
        ScriptedResponse::json(200, json!({"id":"400","bot":"true"})),
    ];
    for id in [
        json!("0"),
        json!("0400"),
        json!("+400"),
        json!(400),
        json!("18446744073709551616"),
    ] {
        responses.push(ScriptedResponse::json(200, json!({"id":id,"bot":true})));
    }
    for response in responses {
        let mock = MockRest::start(vec![], response).await;
        assert!(executor(&mock).current_bot_user_id().await.is_err());
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/api/v10/users/@me");
        mock.shutdown().await;
    }
}

fn permission_channel(overwrites: Vec<Value>) -> Value {
    json!({"id":"700","guild_id":"100","type":0,"permission_overwrites":overwrites})
}

fn overwrite(id: &str, kind: u8, allow: u64, deny: u64) -> Value {
    json!({"id":id,"type":kind,"allow":allow.to_string(),"deny":deny.to_string()})
}

fn member_roles_response() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"user":{"id":"400"},"roles":["300","301"]}))
}

fn guild_roles_response(everyone: u64, bot_role: u64) -> ScriptedResponse {
    ScriptedResponse::json(
        200,
        json!([
            {"id":"100","permissions":everyone.to_string()},
            {"id":"300","permissions":bot_role.to_string()},
            {"id":"301","permissions":"0"},
            {"id":"302","permissions":"0"},
        ]),
    )
}

#[tokio::test]
async fn history_permission_member_grants_need_no_role_fetch_but_validate_entire_channel() {
    let required = (Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY).bits();
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    let channel = permission_channel(vec![
        overwrite("100", 0, 0, required),
        overwrite("400", 1, required, 0),
    ]);
    assert!(rest
        .ticket_history_readable("100", "400", &channel)
        .await
        .unwrap());
    assert!(mock.requests().is_empty());
    for malformed in [
        json!({"id":"300","type":0,"allow":"bad","deny":"0"}),
        json!({"id":"300","type":2,"allow":"0","deny":"0"}),
        json!({"id":"0","type":0,"allow":"0","deny":"0"}),
        json!({"id":"400","type":1,"allow":"0","deny":"0"}),
    ] {
        let mut channel = channel.clone();
        channel["permission_overwrites"]
            .as_array_mut()
            .unwrap()
            .push(malformed);
        assert!(rest
            .ticket_history_readable("100", "400", &channel)
            .await
            .is_err());
    }
    let mut wrong_guild = channel.clone();
    wrong_guild["guild_id"] = json!("101");
    assert!(rest
        .ticket_history_readable("100", "400", &wrong_guild)
        .await
        .is_err());
    let mut missing = channel;
    missing
        .as_object_mut()
        .unwrap()
        .remove("permission_overwrites");
    assert!(rest
        .ticket_history_readable("100", "400", &missing)
        .await
        .is_err());
    assert!(
        mock.requests().is_empty(),
        "malformed evidence fails before REST"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn history_permission_resolves_ordinary_panel_roles_and_discord_overwrite_precedence() {
    let view = Permissions::VIEW_CHANNEL.bits();
    let history = Permissions::READ_MESSAGE_HISTORY.bits();
    let required = view | history;
    for (name, everyone, bot_role, rows, readable) in [
        ("guild everyone grants", required, 0, vec![], true),
        ("held guild role grants", 0, required, vec![], true),
        (
            "everyone overwrite grants",
            0,
            0,
            vec![overwrite("100", 0, required, 0)],
            true,
        ),
        (
            "held role rescues everyone deny",
            required,
            0,
            vec![
                overwrite("100", 0, 0, required),
                overwrite("300", 0, required, 0),
            ],
            true,
        ),
        (
            "role union allow beats role deny",
            required,
            0,
            vec![
                overwrite("300", 0, 0, history),
                overwrite("301", 0, history, 0),
            ],
            true,
        ),
        (
            "unheld role cannot grant",
            view,
            0,
            vec![overwrite("302", 0, history, 0)],
            false,
        ),
        (
            "member deny overrides roles",
            required,
            0,
            vec![overwrite("400", 1, 0, history)],
            false,
        ),
        (
            "member partial allow rescues role deny",
            required,
            0,
            vec![
                overwrite("300", 0, 0, history),
                overwrite("400", 1, history, 0),
            ],
            true,
        ),
        (
            "view without history is insufficient",
            view,
            0,
            vec![],
            false,
        ),
        (
            "history without view is insufficient",
            history,
            0,
            vec![],
            false,
        ),
        (
            "admin bypasses overwrites",
            0,
            Permissions::ADMINISTRATOR.bits(),
            vec![
                overwrite("100", 0, 0, required),
                overwrite("400", 1, 0, required),
            ],
            true,
        ),
    ] {
        let mock = MockRest::start(
            vec![
                member_roles_response(),
                guild_roles_response(everyone, bot_role),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        assert_eq!(
            executor(&mock)
                .ticket_history_readable("100", "400", &permission_channel(rows))
                .await
                .unwrap(),
            readable,
            "{name}"
        );
        let requests = mock.requests();
        assert_eq!(requests.len(), 2, "{name}");
        assert_eq!(requests[0].path, "/api/v10/guilds/100/members/400");
        assert_eq!(requests[1].path, "/api/v10/guilds/100/roles");
        assert!(requests.iter().all(|r| r.method == "GET"));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn history_permission_rejects_missing_or_unreadable_role_evidence() {
    for (member, roles, requests) in [
        (ScriptedResponse::status(403), guild_roles_response(0, 0), 1),
        (
            ScriptedResponse::json(200, json!({"user":{"id":"401"},"roles":[]})),
            guild_roles_response(0, 0),
            1,
        ),
        (
            ScriptedResponse::json(200, json!({"user":{"id":"400"}})),
            guild_roles_response(0, 0),
            1,
        ),
        (member_roles_response(), ScriptedResponse::status(500), 2),
        (
            member_roles_response(),
            ScriptedResponse::json(200, json!([])),
            2,
        ),
        (
            member_roles_response(),
            ScriptedResponse::json(
                200,
                json!([
                    {"id":"100","permissions":"0"}, {"id":"300","permissions":"0"}
                ]),
            ),
            2,
        ),
        (
            member_roles_response(),
            ScriptedResponse::json(
                200,
                json!([
                    {"id":"100","permissions":"0"}, {"id":"300","permissions":8}, {"id":"301","permissions":"0"}
                ]),
            ),
            2,
        ),
    ] {
        let mock = MockRest::start(vec![member, roles], ScriptedResponse::status(500)).await;
        assert!(executor(&mock)
            .ticket_history_readable("100", "400", &permission_channel(vec![]))
            .await
            .is_err());
        assert_eq!(mock.requests().len(), requests, "bounded single attempts");
        assert!(mock.requests().iter().all(|r| r.method == "GET"));
        mock.shutdown().await;
    }
}
