//! Builder-owned coverage for the bounded authority diagnostic vocabulary.
#![cfg(test)]

use super::*;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use two_bot_discord::{ratelimit_guard::GuardError, DiscordError};

struct AuthorityRest {
    executor: ActionExecutor,
    task: tokio::task::JoinHandle<()>,
}

impl AuthorityRest {
    async fn start(responses: Vec<(&'static str, u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
        let task = tokio::spawn(async move {
            for (path, status, body) in responses {
                let (mut stream, _) = listener.accept().await.expect("fixture request");
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).await.expect("read request");
                    assert_ne!(n, 0, "complete request headers");
                    request.extend_from_slice(&buf[..n]);
                    assert!(request.len() <= 8192, "bounded fixture headers");
                }
                assert!(
                    String::from_utf8_lossy(&request).starts_with(&format!("GET {path} HTTP/1.1"))
                );
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("fixture response");
            }
        });
        Self {
            executor: ActionExecutor::with_proxy("fixture-only-token".into(), Some(origin))
                .expect("loopback executor"),
            task,
        }
    }
}

impl Drop for AuthorityRest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn diagnostic_guild() -> serde_json::Value {
    json!({"id":"2222", "owner_id":"99", "roles":[
        {"id":"2222", "position":0, "permissions":"0"},
        {"id":"101", "position":50, "permissions":"6"},
        {"id":"103", "position":100, "permissions":"8"}
    ]})
}

fn diagnostic_execution() -> MemberExecution {
    MemberExecution {
        action: ModerationAction::Warn,
        guild_id: "2222".into(),
        actor: ModerationActor {
            user_id: "88".into(),
            role_ids: vec![],
            highest_role_position: 0,
            permissions: 0,
        },
        target: Some(ModerationTarget {
            user_id: "3333".into(),
            role_ids: vec![],
            highest_role_position: 0,
            is_bot: false,
            is_guild_owner: false,
        }),
        bot_highest_role_position: 0,
        reason: "fixture reason".into(),
        duration_seconds: None,
        request_id: "fixture-request".into(),
        idempotency_key: "fixture-key".into(),
    }
}

#[tokio::test]
async fn current_authority_preserves_unknown_bot_and_unknown_held_roles_causes() {
    for (actor_roles, target_roles, bot_roles, bot_flag, expected) in [
        (
            json!(["101"]),
            json!([]),
            json!(["103"]),
            false,
            AuthorityCause::UnknownBot,
        ),
        (
            json!(["999"]),
            json!([]),
            json!(["103"]),
            true,
            AuthorityCause::MalformedRoles,
        ),
        (
            json!(["101"]),
            json!(["999"]),
            json!(["103"]),
            true,
            AuthorityCause::MalformedRoles,
        ),
        (
            json!(["101"]),
            json!([]),
            json!(["999"]),
            true,
            AuthorityCause::MalformedRoles,
        ),
    ] {
        let rest = AuthorityRest::start(vec![
            ("/api/v10/guilds/2222", 200, diagnostic_guild().to_string()),
            (
                "/api/v10/guilds/2222/members/88",
                200,
                json!({"user":{"id":"88"}, "roles":actor_roles}).to_string(),
            ),
            (
                "/api/v10/guilds/2222/members/3333",
                200,
                json!({"user":{"id":"3333"}, "roles":target_roles}).to_string(),
            ),
            (
                "/api/v10/guilds/2222/members/4444",
                200,
                json!({"user":{"id":"4444", "bot":bot_flag}, "roles":bot_roles}).to_string(),
            ),
        ])
        .await;
        match current_authority(&rest.executor, &diagnostic_execution(), "4444").await {
            Err(AuthorityError::Unavailable(cause)) => assert_eq!(cause, expected),
            _ => panic!("expected the pinned unavailable cause"),
        }
    }
}

#[tokio::test]
async fn guild_reads_attribute_missing_mismatched_and_malformed_evidence() {
    let mut bodies = vec![
        (404, "{}".into(), AuthorityCause::MalformedRoles),
        (
            200,
            "not JSON: hostile sentinel".into(),
            AuthorityCause::MalformedRoles,
        ),
        (
            200,
            json!({"id":"9999"}).to_string(),
            AuthorityCause::GuildMismatch,
        ),
        (200, "{}".into(), AuthorityCause::GuildMismatch),
    ];
    for (field, value) in [
        ("owner_id", json!("0")),
        ("roles", json!(null)),
        (
            "roles",
            json!([{"id":"0", "position":0, "permissions":"0"}]),
        ),
        (
            "roles",
            json!([{"id":"2222", "position":-1, "permissions":"0"}]),
        ),
        (
            "roles",
            json!([{"id":"2222", "position":0, "permissions":null}]),
        ),
        (
            "roles",
            json!([{"id":"2222", "position":0, "permissions":"hostile sentinel"}]),
        ),
        (
            "roles",
            json!([
                {"id":"2222", "position":0, "permissions":"0"},
                {"id":"2222", "position":1, "permissions":"0"}
            ]),
        ),
        ("roles", json!([])),
    ] {
        let mut body = diagnostic_guild();
        body[field] = value;
        bodies.push((200, body.to_string(), AuthorityCause::MalformedRoles));
    }
    for (status, body, expected) in bodies {
        let rest = AuthorityRest::start(vec![("/api/v10/guilds/2222", status, body)]).await;
        assert!(
            matches!(guild_facts(&rest.executor, "2222").await, Err(cause) if cause == expected)
        );
    }
}

#[tokio::test]
async fn member_reads_preserve_missing_membership_and_json_failure() {
    for (status, body, expected) in [
        (404, "{}", None),
        (
            200,
            "not JSON: hostile sentinel",
            Some(AuthorityCause::MalformedRoles),
        ),
        (403, "{}", Some(AuthorityCause::Discord403)),
        (401, "{}", Some(AuthorityCause::Transport)),
    ] {
        let rest = AuthorityRest::start(vec![(
            "/api/v10/guilds/2222/members/88",
            status,
            body.into(),
        )])
        .await;
        let result = fetched_member(&rest.executor, "2222", "88").await;
        match expected {
            Some(cause) => assert_eq!(result, Err(cause)),
            None => assert_eq!(result, Ok(None)),
        }
    }
}

#[test]
fn authority_read_errors_keep_fixed_causes_without_echoing_details() {
    for (error, expected) in [
        (
            DiscordError::Rejected("Discord refused the request with 403".into()),
            AuthorityCause::Discord403,
        ),
        (DiscordError::Timeout, AuthorityCause::Transport),
        (
            DiscordError::Unavailable("Discord returned 503".into()),
            AuthorityCause::Transport,
        ),
        (DiscordError::RateLimited, AuthorityCause::Transport),
        (
            DiscordError::Guard(GuardError::CircuitOpen),
            AuthorityCause::Transport,
        ),
        (
            DiscordError::Unavailable("invalid JSON response".into()),
            AuthorityCause::MalformedRoles,
        ),
        (
            DiscordError::Rejected("Discord refused the request with 401".into()),
            AuthorityCause::Transport,
        ),
        (
            DiscordError::Unavailable("hostile body contains 403\nsecret sentinel".into()),
            AuthorityCause::Transport,
        ),
        (
            DiscordError::Rejected("untrusted detail contains 403".into()),
            AuthorityCause::Transport,
        ),
    ] {
        assert_eq!(AuthorityCause::from_read(&error), expected);
        assert!(!expected.token().contains("sentinel"));
    }
}

#[tokio::test]
async fn every_identity_lookup_failure_is_unknown_bot() {
    for (status, body) in [
        (500, "{}"),
        (403, "{}"),
        (404, "{}"),
        (200, "not JSON: hostile sentinel"),
        (200, r#"{"id":"4444","bot":false}"#),
        (200, r#"{"id":"0","bot":true}"#),
    ] {
        let rest = AuthorityRest::start(vec![("/api/v10/users/@me", status, body.into())]).await;
        // This lazy pool is never queried by bot_id; no database is needed.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_unused")
            .expect("lazy fixture");
        let runtime = MemberRuntime::for_test(pool.clone(), "2222");
        assert_eq!(
            runtime.bot_id(&rest.executor).await,
            Err(AuthorityCause::UnknownBot)
        );
        assert!(
            runtime.bot_id.get().is_none(),
            "failures never cache a guessed identity"
        );
        pool.close().await;
    }
}

#[test]
fn hostile_member_evidence_has_only_the_malformed_roles_cause() {
    for body in [
        json!(null),
        json!({"user": {"id": "88"}, "roles": ["0"]}),
        json!({"user": {"id": "88"}, "roles": [42]}),
        json!({"user": {"id": "88"}, "roles": ["01"]}),
        json!({"user": {"id": "88"}, "roles": ["hostile\nsecret sentinel"]}),
        json!({"user": {"id": "88", "bot": "true"}, "roles": []}),
        json!({"user": {"id": "99"}, "roles": []}),
        json!({"user": {"id": "88"}, "roles": {"101": true}}),
    ] {
        assert_eq!(
            parse_member(&body, "88"),
            Err(AuthorityCause::MalformedRoles)
        );
    }
}

#[test]
fn cause_tokens_are_bounded_and_do_not_change_member_replies_or_outcomes() {
    let tokens: Vec<_> = [
        AuthorityCause::Discord403,
        AuthorityCause::Transport,
        AuthorityCause::MalformedRoles,
        AuthorityCause::UnknownBot,
        AuthorityCause::GuildMismatch,
    ]
    .into_iter()
    .map(AuthorityCause::token)
    .collect();
    assert_eq!(
        tokens,
        [
            "discord_403",
            "transport",
            "malformed_roles",
            "unknown_bot",
            "guild_mismatch"
        ]
    );
    for cause in [
        AuthorityCause::Discord403,
        AuthorityCause::Transport,
        AuthorityCause::MalformedRoles,
        AuthorityCause::UnknownBot,
        AuthorityCause::GuildMismatch,
    ] {
        let mut recorded = None;
        let error = AuthorityError::Unavailable(cause).into_member_error(&mut recorded);
        assert_eq!(recorded, Some(cause));
        assert_eq!(error.enforcement_outcome(), "refused");
        assert_eq!(error_text(error), "Moderation command failed; try again.");
    }
    let mut recorded = None;
    let error =
        AuthorityError::Member(MemberError::TargetNotMember).into_member_error(&mut recorded);
    assert!(recorded.is_none());
    assert!(matches!(error, MemberError::TargetNotMember));
}

#[test]
fn authority_warn_sampling_is_per_cause_and_reopens_at_sixty_seconds() {
    let mut warnings = AuthorityWarnings::default();
    let now = Instant::now();
    let causes = [
        AuthorityCause::Discord403,
        AuthorityCause::Transport,
        AuthorityCause::MalformedRoles,
        AuthorityCause::UnknownBot,
        AuthorityCause::GuildMismatch,
    ];
    for cause in causes {
        assert!(warnings.admit(cause, now));
        for _ in 0..10_000 {
            assert!(!warnings.admit(cause, now));
        }
        assert!(!warnings.admit(cause, now + Duration::from_secs(59)));
        assert!(warnings.admit(cause, now + Duration::from_secs(60)));
        assert!(!warnings.admit(cause, now + Duration::from_secs(60)));
    }
    assert_eq!(
        warnings.last.len(),
        5,
        "no member-keyed sampling allocation"
    );
}
