//! Acceptance: the session picker's "Join voice now" option hands out the
//! lobby link (and records a successful route) only when the member can both
//! view and connect to the lobby. A visible-but-not-connectable lobby is
//! withheld exactly like any unavailable destination, while an allowed text
//! destination in the same submission still routes.
//!
//! Protected acceptance file: the paired implementation must make the
//! Connect-denied cases pass without editing this file. This file brings its
//! own mock Discord responder so fixture changes in the sibling runtime
//! tests cannot change what is pinned here. Channel 10 is the looking-to-play
//! text room; channel 11 is the lobby voice room (see `super::vars`).

use super::*;

/// VIEW_CHANNEL | SEND_MESSAGES | CONNECT for @everyone unless a test says
/// otherwise: the lobby is connectable by default, overwrites decide.
const EVERYONE_VIEW_SEND_CONNECT: &str = "1051648";
const CONNECT_DENY: &str = "1048576";
const VIEW_DENY: &str = "1024";

async fn voice_discord(
    everyone_permissions: &str,
    member_roles: Vec<String>,
    lobby_overwrites: Value,
) -> MockRest {
    let everyone_permissions = everyone_permissions.to_owned();
    MockRest::with_responder(move |request: &RestRequest| {
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/guilds/22") => {
                ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"}))
            }
            ("GET", "/guilds/22/roles") => {
                let mut roles = vec![
                    json!({"id":"22","permissions":everyone_permissions}),
                    json!({"id":"55","permissions":"0"}),
                    json!({"id":"77","permissions":"0"}),
                ];
                roles.extend(
                    GAME_PICKS
                        .iter()
                        .chain(PLATFORM_PICKS.iter())
                        .map(|pick| json!({"id":pick.role_id,"permissions":"0"})),
                );
                ScriptedResponse::json(200, Value::Array(roles))
            }
            ("GET", "/guilds/22/members/44") => ScriptedResponse::json(
                200,
                json!({"user":{"id":"44"},"roles":member_roles}),
            ),
            ("GET", "/channels/10") => ScriptedResponse::json(
                200,
                json!({"id":"10","guild_id":"22","type":0,"permission_overwrites":[]}),
            ),
            ("GET", "/channels/11") => ScriptedResponse::json(
                200,
                json!({"id":"11","guild_id":"22","type":2,"permission_overwrites":lobby_overwrites.clone()}),
            ),
            ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
            ("PATCH", path) if path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

async fn last_routed_metadata(db: &TestSchema) -> Value {
    let metadata: String = sqlx::query_scalar("SELECT metadata FROM events WHERE event_type = $1")
        .bind(EVENT_CHANNEL_ROUTED)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    serde_json::from_str(&metadata).unwrap()
}

fn last_reply(mock: &MockRest) -> String {
    let reply: Value = serde_json::from_slice(&replies(mock).last().unwrap().body).unwrap();
    reply["content"].as_str().unwrap().to_owned()
}

// RED at baseline: the member overwrite denies CONNECT on a visible lobby,
// yet the picker replies with the lobby link and records a successful route.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_session_join_voice_member_connect_deny_withholds_link_and_route_fact() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = voice_discord(
            EVERYONE_VIEW_SEND_CONNECT,
            vec![],
            json!([{"id":"44","type":1,"allow":"0","deny":CONNECT_DENY}]),
        )
        .await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime
            .handle(component(SESSION_SELECT_ID, &["join-voice"]), NOW)
            .await
            .unwrap();
        let content = last_reply(&mock);
        assert!(
            !content.contains("<#11>"),
            "no actionable voice-room link for a Connect-denied member: {content}"
        );
        assert!(
            content.contains("not open to you"),
            "the withheld lobby is reported like any unavailable room: {content}"
        );
        assert_eq!(
            db.count(EVENT_CHANNEL_ROUTED).await,
            0,
            "no successful voice-route fact may be recorded"
        );
        assert!(
            !mock
                .requests()
                .iter()
                .any(|request| request.path == "/api/v10/channels/10"),
            "only the selected destination is read"
        );
        assert!(!mock
            .requests()
            .iter()
            .any(|request| matches!(request.method.as_str(), "PUT" | "DELETE")));
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: with an @everyone Connect deny, a mixed submission routes
// both rooms; only the text room may route, with the voice pick reported
// unavailable, catalog order and deduplication preserved.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_session_join_voice_everyone_connect_deny_still_routes_text_pick() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = voice_discord(
            EVERYONE_VIEW_SEND_CONNECT,
            vec![],
            json!([{"id":"22","type":0,"allow":"0","deny":CONNECT_DENY}]),
        )
        .await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime
            .handle(
                component(
                    SESSION_SELECT_ID,
                    &["join-voice", "find-players", "join-voice"],
                ),
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(last_reply(&mock), "On it - head to <#10>.");
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
        let routed = last_routed_metadata(&db).await;
        assert_eq!(routed["picks"], json!(["find-players", "join-voice"]));
        assert_eq!(routed["channels"], json!(["10"]));
        assert_eq!(routed["unavailable"], json!(["join-voice"]));
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Control: View+Connect allowed keeps today's successful voice route.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_session_join_voice_view_and_connect_allowed_routes() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = voice_discord(EVERYONE_VIEW_SEND_CONNECT, vec![], json!([])).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime
            .handle(component(SESSION_SELECT_ID, &["join-voice"]), NOW)
            .await
            .unwrap();
        assert_eq!(last_reply(&mock), "On it - head to <#11>.");
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
        let routed = last_routed_metadata(&db).await;
        assert_eq!(routed["picks"], json!(["join-voice"]));
        assert_eq!(routed["channels"], json!(["11"]));
        assert_eq!(routed["unavailable"], json!([]));
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Control: a role Connect allow overrides the @everyone Connect deny, per
// the existing overwrite precedence, and the route succeeds.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_session_join_voice_role_connect_allow_routes() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = voice_discord(
            EVERYONE_VIEW_SEND_CONNECT,
            vec!["55".into()],
            json!([
                {"id":"22","type":0,"allow":"0","deny":CONNECT_DENY},
                {"id":"55","type":0,"allow":CONNECT_DENY,"deny":"0"}
            ]),
        )
        .await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime
            .handle(component(SESSION_SELECT_ID, &["join-voice"]), NOW)
            .await
            .unwrap();
        assert_eq!(last_reply(&mock), "On it - head to <#11>.");
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Control: View denied withholds the lobby even when Connect is allowed.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_session_join_voice_view_deny_withholds_link_and_route_fact() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = voice_discord(
            EVERYONE_VIEW_SEND_CONNECT,
            vec![],
            json!([{"id":"44","type":1,"allow":"0","deny":VIEW_DENY}]),
        )
        .await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime
            .handle(component(SESSION_SELECT_ID, &["join-voice"]), NOW)
            .await
            .unwrap();
        let content = last_reply(&mock);
        assert!(!content.contains("<#11>"), "{content}");
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 0);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
