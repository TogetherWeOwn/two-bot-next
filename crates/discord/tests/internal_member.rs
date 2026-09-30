// Shared fixture also contains a gateway double unused by this REST-only suite.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use two_bot_core::internal_actions::{ErrorCode, GuildAddMemberRequest, RoleAssignRequest};
use two_bot_discord::executor::member::MemberOutcome;
use two_bot_discord::ActionExecutor;

const GUILD: &str = "100000000000000001";
const USER: &str = "100000000000000002";
const BOT: &str = "100000000000000003";
const ROLE: &str = "100000000000000004";
const BOT_ROLE: &str = "100000000000000005";
const TOKEN: &str = "fixture-only-oauth-DO-NOT-LOG";

fn add_body() -> Map<String, Value> {
    json!({"action":"guild.add_member","discord_id": USER,"access_token":TOKEN})
        .as_object()
        .unwrap()
        .clone()
}
fn role_body() -> Map<String, Value> {
    json!({"action":"role.assign","discord_id":USER,"role_key":"member"})
        .as_object()
        .unwrap()
        .clone()
}
fn keys() -> HashMap<String, String> {
    HashMap::from([("member".into(), ROLE.into())])
}
fn roles(position: i64, managed: bool) -> ScriptedResponse {
    ScriptedResponse::json(
        200,
        json!([
            {"id":GUILD,"position":0,"managed":false},
            {"id":ROLE,"position":position,"managed":managed},
            {"id":BOT_ROLE,"position":10,"managed":true}
        ]),
    )
}
fn policy_script(position: i64, managed: bool) -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::json(200, json!({"roles":[]})),
        ScriptedResponse::json(200, json!({"roles":[BOT_ROLE]})),
        roles(position, managed),
    ]
}
fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("fixture-bot-token".into(), Some(mock.origin())).unwrap()
}

#[tokio::test]
async fn add_member_distinguishes_created_and_present_with_exact_wire_json() {
    for (status, outcome, wire) in [
        (
            201,
            MemberOutcome::Added,
            r#"{"ok":true,"result":{"outcome":"added"},"request_id":"req"}"#,
        ),
        (
            204,
            MemberOutcome::AlreadyMember,
            r#"{"ok":true,"result":{"outcome":"already_member"},"request_id":"req"}"#,
        ),
        (
            200,
            MemberOutcome::AlreadyMember,
            r#"{"ok":true,"result":{"outcome":"already_member"},"request_id":"req"}"#,
        ),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::status(status)],
            ScriptedResponse::status(500),
        )
        .await;
        let body = add_body();
        let request = GuildAddMemberRequest::validate(&body).unwrap();
        let actual = executor(&mock)
            .add_internal_member(GUILD, &request, TOKEN)
            .await
            .unwrap();
        assert_eq!(actual, outcome);
        assert_eq!(actual.success_body("req").to_string(), wire);
        let calls = mock.requests();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "PUT");
        assert_eq!(
            calls[0].path,
            format!("/api/v10/guilds/{GUILD}/members/{USER}")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&calls[0].body).unwrap(),
            json!({"access_token": TOKEN})
        );
        assert_eq!(
            calls[0].header("authorization"),
            Some("Bot fixture-bot-token")
        );
        assert!(!format!("{request:?}").contains(TOKEN));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn refusals_match_legacy_codes_and_ignore_provider_body() {
    for (status, code) in [
        (403, ErrorCode::DiscordRejected),
        (404, ErrorCode::DiscordRejected),
        (400, ErrorCode::DiscordRejected),
        (503, ErrorCode::DiscordUnavailable),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(status, json!({"message": TOKEN}))],
            ScriptedResponse::status(201),
        )
        .await;
        let body = add_body();
        let error = executor(&mock)
            .add_internal_member(
                GUILD,
                &GuildAddMemberRequest::validate(&body).unwrap(),
                TOKEN,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, code);
        assert!(!format!("{error:?}").contains(TOKEN));
        if status == 403 {
            assert!(error.message.contains("highest role"));
        }
        if status == 404 {
            assert_eq!(error.message, "Discord refused the request with 404");
        }
        assert_eq!(mock.requests().len(), 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn redirects_refuse_both_mutations_without_forwarding_credentials() {
    let target = MockRest::start(vec![], ScriptedResponse::status(201)).await;
    for status in [301, 302, 303, 307, 308] {
        let mut redirect = ScriptedResponse::json(status, json!({"message": TOKEN}));
        redirect.headers.push(("location".into(), target.origin()));
        for role_action in [false, true] {
            let mut script = if role_action {
                policy_script(2, false)
            } else {
                vec![]
            };
            script.push(redirect.clone());
            let mock = MockRest::start(script, ScriptedResponse::status(201)).await;
            let exec = executor(&mock);
            let error = if role_action {
                let body = role_body();
                let keys = keys();
                exec.assign_internal_role(
                    GUILD,
                    BOT,
                    &RoleAssignRequest::validate(&body, &keys).unwrap(),
                )
                .await
                .unwrap_err()
            } else {
                let body = add_body();
                exec.add_internal_member(
                    GUILD,
                    &GuildAddMemberRequest::validate(&body).unwrap(),
                    TOKEN,
                )
                .await
                .unwrap_err()
            };
            assert_eq!(error.code, ErrorCode::DiscordUnavailable);
            assert_eq!(error.log_reason, "discord_unexpected_status");
            assert!(!format!("{error:?}").contains(TOKEN));
            assert_eq!(mock.requests().len(), if role_action { 4 } else { 1 });
            assert!(target.requests().is_empty());
            mock.shutdown().await;
        }
    }
    target.shutdown().await;
}

#[tokio::test]
async fn rate_limit_returns_header_retry_delay_without_hidden_retry() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::rate_limited(99.0, "0.5"),
            ScriptedResponse::status(201),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let body = add_body();
    let request = GuildAddMemberRequest::validate(&body).unwrap();
    let exec = executor(&mock);
    let error = exec
        .add_internal_member(GUILD, &request, TOKEN)
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::RateLimited);
    assert_eq!(error.retry_after_secs, Some(1));
    assert!(error.code.retryable());
    assert_eq!(mock.requests().len(), 1);
    // A deliberate caller retry, not an automatic transport retry.
    assert_eq!(
        exec.add_internal_member(GUILD, &request, TOKEN)
            .await
            .unwrap(),
        MemberOutcome::Added
    );
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn role_assignment_reads_policy_then_sends_empty_put() {
    let mut script = policy_script(2, false);
    script.push(ScriptedResponse::status(204));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let body = role_body();
    let keys = keys();
    let result = executor(&mock)
        .assign_internal_role(
            GUILD,
            BOT,
            &RoleAssignRequest::validate(&body, &keys).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result, MemberOutcome::Assigned);
    assert_eq!(
        result.success_body("req").to_string(),
        r#"{"ok":true,"result":{"outcome":"assigned"},"request_id":"req"}"#
    );
    let calls = mock.requests();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[2].path, format!("/api/v10/guilds/{GUILD}/roles"));
    assert_eq!(calls[3].method, "PUT");
    assert_eq!(
        calls[3].path,
        format!("/api/v10/guilds/{GUILD}/members/{USER}/roles/{ROLE}")
    );
    assert!(calls[3].body.is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn role_already_held_avoids_mutation() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!({"roles":[ROLE]}))],
        ScriptedResponse::status(500),
    )
    .await;
    let body = role_body();
    let keys = keys();
    let result = executor(&mock)
        .assign_internal_role(
            GUILD,
            BOT,
            &RoleAssignRequest::validate(&body, &keys).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result, MemberOutcome::AlreadyHeld);
    assert_eq!(
        result.success_body("req").to_string(),
        r#"{"ok":true,"result":{"outcome":"already_held"},"request_id":"req"}"#
    );
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn unsafe_hierarchy_and_managed_roles_refuse_before_put() {
    for (position, managed) in [(10, false), (11, false), (2, true)] {
        let mock = MockRest::start(
            policy_script(position, managed),
            ScriptedResponse::status(204),
        )
        .await;
        let body = role_body();
        let keys = keys();
        let error = executor(&mock)
            .assign_internal_role(
                GUILD,
                BOT,
                &RoleAssignRequest::validate(&body, &keys).unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::DiscordRejected);
        assert_eq!(mock.requests().len(), 3);
        assert!(mock.requests().iter().all(|r| r.method == "GET"));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn role_put_maps_upstream_hierarchy_and_unknown_user() {
    for status in [403, 404] {
        let mut script = policy_script(2, false);
        script[0] = ScriptedResponse::status(404); // legacy read-failure fallback
        script.push(ScriptedResponse::status(status));
        let mock = MockRest::start(script, ScriptedResponse::status(204)).await;
        let body = role_body();
        let keys = keys();
        let error = executor(&mock)
            .assign_internal_role(
                GUILD,
                BOT,
                &RoleAssignRequest::validate(&body, &keys).unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::DiscordRejected);
        assert!(error.message.contains(&status.to_string()));
        assert_eq!(mock.requests().len(), 4);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn unreadable_policy_and_unknown_allowlist_fail_closed() {
    let body = role_body();
    assert_eq!(
        RoleAssignRequest::validate(&body, &HashMap::new())
            .unwrap_err()
            .code,
        ErrorCode::ActionNotAllowed
    );
    let keys = keys();
    for response in [
        ScriptedResponse::status(503),
        ScriptedResponse::json(200, json!({"wrong":"shape"})),
        ScriptedResponse::json(200, json!([{"id":ROLE,"position":2}])),
    ] {
        let mut script = policy_script(2, false);
        script[2] = response;
        let mock = MockRest::start(script, ScriptedResponse::status(204)).await;
        let error = executor(&mock)
            .assign_internal_role(
                GUILD,
                BOT,
                &RoleAssignRequest::validate(&body, &keys).unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::DiscordUnavailable);
        assert!(mock.requests().iter().all(|r| r.method == "GET"));
        mock.shutdown().await;
    }
}

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn oauth_token_never_appears_in_captured_tracing_even_on_timeout_and_error() {
    let output = Arc::new(Mutex::new(vec![]));
    let writer = LogWriter(output.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    for response in [
        ScriptedResponse::status(201),
        ScriptedResponse::status(204),
        ScriptedResponse::json(403, json!({"message": TOKEN})),
        ScriptedResponse::json(404, json!({"message": TOKEN})),
        ScriptedResponse {
            headers: vec![(
                "location".into(),
                format!("https://fixture.invalid/{TOKEN}"),
            )],
            ..ScriptedResponse::json(307, json!({"message": TOKEN}))
        },
        ScriptedResponse::rate_limited(0.1, "1"),
        ScriptedResponse::json(201, json!({"token":TOKEN}))
            .delayed(std::time::Duration::from_millis(1600)),
    ] {
        let mock = MockRest::start(vec![response], ScriptedResponse::status(500)).await;
        let body = add_body();
        let result = executor(&mock)
            .add_internal_member(
                GUILD,
                &GuildAddMemberRequest::validate(&body).unwrap(),
                TOKEN,
            )
            .await;
        tracing::info!(?result, "captured outcome");
        assert!(!format!("{result:?}").contains(TOKEN));
        mock.shutdown().await;
    }
    let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("captured outcome"));
    assert!(logs.contains("discord_rate_limited"));
    assert!(!logs.contains(TOKEN), "OAuth token leaked into tracing");
}
