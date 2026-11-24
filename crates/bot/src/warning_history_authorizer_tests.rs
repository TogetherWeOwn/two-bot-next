use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::json;
use std::sync::Arc;
use two_bot_discord::ratelimit_guard::{GuardConfig, RateLimitGuard};

const PATHS: [&str; 5] = [
    "/api/v10/guilds/22",
    "/api/v10/guilds/22/members/44",
    "/api/v10/users/@me",
    "/api/v10/guilds/22/members/99",
    "/api/v10/channels/77",
];
const VIEW: u64 = Permissions::VIEW_CHANNEL.bits();
const COMMANDS: u64 = Permissions::USE_SLASH_COMMANDS.bits();

fn evidence() -> [Value; 5] {
    [
        json!({"id":"22","owner_id":"55","roles":[
            {"id":"22","position":0,"permissions":VIEW.to_string()},
            {"id":"33","position":1,"permissions":(PERM_MODERATE_MEMBERS | COMMANDS).to_string()},
            {"id":"34","position":2,"permissions":"0"}
        ]}),
        json!({"user":{"id":"44"},"roles":["33","34"],"communication_disabled_until":null}),
        json!({"id":"99","bot":true}),
        json!({"user":{"id":"99","bot":true},"roles":[]}),
        json!({"id":"77","guild_id":"22","type":0,"parent_id":"66","permission_overwrites":[]}),
    ]
}

fn overwrite(id: &str, kind: u8, allow: u64, deny: u64) -> Value {
    json!({"id":id,"type":kind,"allow":allow.to_string(),"deny":deny.to_string()})
}

fn executor(rest: &MockRest) -> ActionExecutor {
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).unwrap());
    ActionExecutor::with_proxy_and_guard("fixture-only".into(), Some(rest.origin()), guard).unwrap()
}

async fn refresh(
    rest: &MockRest,
    guild: Option<Id<GuildMarker>>,
    target: u64,
    deadline: Instant,
) -> Result<(), Refusal> {
    authorize(
        &executor(rest),
        Id::new(22),
        guild,
        Id::new(77),
        Id::new(44),
        Id::new(target),
        deadline,
    )
    .await
}

fn assert_reads(rest: &MockRest, count: usize) {
    let requests = rest.requests();
    assert_eq!(requests.len(), count);
    for (request, path) in requests.iter().zip(PATHS) {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, path);
        assert!(request.body.is_empty());
    }
}

async fn check(docs: [Value; 5], expected: Result<(), Refusal>, reads: usize) {
    let responses = Vec::from(docs.map(|doc| ScriptedResponse::json(200, doc)));
    let rest = MockRest::start(responses, ScriptedResponse::status(500)).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let decision = refresh(&rest, Some(Id::new(22)), 88, deadline).await;
    assert_eq!(decision, expected);
    assert_reads(&rest, reads);
}

#[tokio::test]
async fn admits_current_staff_with_discord_overwrite_precedence() {
    let mut docs = evidence();
    docs[4]["permission_overwrites"] = json!([
        overwrite("22", 0, 0, VIEW | COMMANDS),
        overwrite("33", 0, 0, VIEW),
        overwrite("34", 0, VIEW, 0),
        overwrite("44", 1, COMMANDS, 0),
        overwrite("99", 1, VIEW, 0),
    ]);
    check(docs.clone(), Ok(()), 5).await;
    docs[4]["permission_overwrites"][3] = overwrite("44", 1, COMMANDS, VIEW);
    check(docs.clone(), Err(Refusal::Denied), 5).await;
    docs[0]["owner_id"] = json!("44");
    docs[1]["roles"] = json!([]);
    check(docs.clone(), Ok(()), 5).await;
    docs[0]["owner_id"] = json!("55");
    docs[0]["roles"][1]["permissions"] = json!("8");
    docs[1]["roles"] = json!(["33"]);
    check(docs, Ok(()), 5).await;
}

#[tokio::test]
async fn refuses_unknown_or_insufficient_live_evidence() {
    use Refusal::{Denied, Unavailable};
    for (slot, field, value, why, reads) in [
        (0, "id", json!("23"), Unavailable, 1),
        (1, "user", json!({"id":"44","bot":true}), Denied, 2),
        (1, "user", json!({"id":"44","system":true}), Denied, 2),
        (1, "roles", json!([]), Denied, 2),
        (1, "roles", json!(["404"]), Unavailable, 2),
        (1, "pending", json!(true), Denied, 2),
        (2, "bot", json!(false), Unavailable, 3),
        (3, "user", json!({"id":"99"}), Unavailable, 4),
        (3, "roles", json!(["404"]), Unavailable, 4),
        (4, "guild_id", json!("23"), Unavailable, 5),
        (4, "type", json!(11), Denied, 5),
    ] {
        let mut docs = evidence();
        docs[slot][field] = value;
        check(docs, Err(why), reads).await;
    }
    for (until, why) in [("invalid", Unavailable), ("2099-01-01T00:00:00Z", Denied)] {
        let mut docs = evidence();
        docs[1]["communication_disabled_until"] = json!(until);
        check(docs, Err(why), 2).await;
    }
    let mut malformed = overwrite("22", 0, 0, 0);
    malformed["deny"] = json!(1024);
    for (overwrites, why) in [
        (Value::Null, Unavailable),
        (json!([malformed]), Unavailable),
        (json!([overwrite("22", 2, 0, 0)]), Unavailable),
        (json!([overwrite("404", 0, 0, 0)]), Unavailable),
        (json!([overwrite("22", 0, 0, 0); 2]), Unavailable),
        (json!([overwrite("44", 1, 0, COMMANDS)]), Denied),
        (json!([overwrite("99", 1, 0, VIEW)]), Denied),
    ] {
        let mut docs = evidence();
        docs[4]["permission_overwrites"] = overwrites;
        check(docs, Err(why), 5).await;
    }
}

#[tokio::test]
async fn slow_reads_share_the_caller_budget_and_refresh_cap_without_retry() {
    for (caller_seconds, outer_seconds, reads) in [(1, 2, 1), (10, 6, 3)] {
        let responses = evidence().map(|doc| ScriptedResponse::json(200, doc));
        let rest = MockRest::start_with_body_delay(
            Vec::from(responses),
            ScriptedResponse::status(500),
            Duration::from_millis(1800),
        )
        .await;
        let now = Instant::now();
        let deadline = now + Duration::from_secs(caller_seconds);
        let attempt = refresh(&rest, Some(Id::new(22)), 88, deadline);
        let decision = timeout_at(now + Duration::from_secs(outer_seconds), attempt).await;
        assert_eq!(decision, Ok(Err(Refusal::Unavailable)));
        assert_reads(&rest, reads);
    }
}

#[tokio::test]
async fn wrong_scope_and_expired_budget_never_fetch_and_failed_reads_never_retry() {
    let rest = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let now = Instant::now();
    let later = now + Duration::from_secs(5);
    for (guild, target) in [(None, 88), (Some(Id::new(23)), 88), (Some(Id::new(22)), 44)] {
        assert_eq!(
            refresh(&rest, guild, target, later).await,
            Err(Refusal::Denied)
        );
    }
    assert_eq!(
        refresh(&rest, Some(Id::new(22)), 88, now).await,
        Err(Refusal::Unavailable)
    );
    assert_reads(&rest, 0);
    for status in [403, 404, 429, 503] {
        let rest = MockRest::start(vec![], ScriptedResponse::status(status)).await;
        let decision = refresh(&rest, Some(Id::new(22)), 88, later).await;
        assert_eq!(decision, Err(Refusal::Unavailable));
        assert_reads(&rest, 1);
    }
}
