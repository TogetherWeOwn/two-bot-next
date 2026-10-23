#![cfg(test)]

use super::*;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use std::sync::{Arc, Mutex};
use two_bot_testsupport::TestDatabase;

#[derive(Default)]
struct State {
    held: HashSet<String>,
    pending: bool,
    bot: bool,
    absent: bool,
    no_manage: bool,
    admin: bool,
    reject_put: bool,
}

async fn database() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("disposable database"),
    )
}

async fn discord(state: Arc<Mutex<State>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let mut state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        let roles = json!([
            {"id":"22", "permissions":"3072", "managed":false, "position":0},
            {"id":"1001", "permissions":"0", "managed":false, "position":1},
            {"id":"1002", "permissions":"0", "managed":true, "position":1},
            {"id":"1003", "permissions":"0", "managed":false, "position":5},
            {"id":"1004", "permissions":"8", "managed":false, "position":1},
            {"id":"1005", "permissions":"0", "managed":false, "position":2},
            {"id":"1006", "permissions":if state.no_manage { "0" } else { "268435456" }, "managed":false, "position":5},
            {"id":"1009", "permissions":"32", "managed":false, "position":2}
        ]);
        match (request.method.as_str(), path) {
            ("GET", "/users/@me") => ScriptedResponse::json(200, json!({"id":"999","bot":true})),
            ("GET", "/guilds/22") => ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"})),
            ("GET", "/guilds/22/roles") => ScriptedResponse::json(200, roles),
            ("GET", "/guilds/22/members/999") => ScriptedResponse::json(200, json!({"user":{"id":"999","bot":true},"roles":["1005","1006"]})),
            ("GET", "/guilds/22/members/44") if state.absent => ScriptedResponse::status(404),
            ("GET", "/guilds/22/members/44") => {
                let mut held: Vec<_> = state.held.iter().cloned().collect();
                if state.admin { held.push("1009".to_owned()); }
                ScriptedResponse::json(200, json!({"user":{"id":"44","bot":state.bot},"pending":state.pending,"roles":held}))
            }
            ("GET", "/channels/12") => ScriptedResponse::json(200, json!({"id":"12","guild_id":"22","type":0,"permission_overwrites":[]})),
            ("PUT", path) if path.starts_with("/guilds/22/members/44/roles/") => {
                if state.reject_put { return ScriptedResponse::status(403); }
                state.held.insert(path.rsplit('/').next().unwrap().to_owned());
                ScriptedResponse::status(204)
            }
            ("POST", "/channels/12/messages") => ScriptedResponse::json(200, json!({"id":"99","channel_id":"12"})),
            ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
            ("PATCH", path) if path.ends_with("/messages/@original") => ScriptedResponse::json(200, json!({"id":"98"})),
            _ => panic!("unexpected mock route {} {path}", request.method),
        }
    }).await
}

fn runtime(db: &TestDatabase, mock: &MockRest, overrides: &[(&str, &str)]) -> OnboardingRuntime {
    crate::gateway::ensure_crypto_provider();
    let executor =
        ActionExecutor::with_proxy("onboarding-builder-fixture".into(), Some(mock.origin()))
            .unwrap();
    let mut vars = HashMap::from([("TWO_ONBOARDING_MODE".into(), "legacy".into())]);
    vars.extend(
        overrides
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into())),
    );
    OnboardingRuntime::new(db.pool().clone(), executor, &vars, 22, 999).unwrap()
}

fn press(id: u64) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"111","authorizing_integration_owners":{"0":"22"},
        "id":id.to_string(),"token":format!("builder-{id}"),"type":3,"version":1,"guild_id":"22",
        "channel":{"id":"12","type":0},
        "member":{"user":{"id":"44","username":"member","discriminator":"0"},"roles":[],"deaf":false,"mute":false,"flags":0,"permissions":"32"},
        "data":{"custom_id":VERIFY_CUSTOM_ID,"component_type":2}
    })).unwrap()
}

fn admin(id: u64, name: &str, options: serde_json::Value) -> Interaction {
    let mut value = serde_json::to_value(press(id)).unwrap();
    value["type"] = json!(2);
    value["data"] = json!({"id":"77","name":"onboarding","type":1,"options":[{"name":name,"type":1,"options":options}]});
    serde_json::from_value(value).unwrap()
}

fn reply(mock: &MockRest) -> String {
    let request = mock
        .requests()
        .into_iter()
        .rfind(|request| request.method == "PATCH")
        .expect("deferred reply");
    serde_json::from_slice::<serde_json::Value>(&request.body).unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn puts(mock: &MockRest) -> usize {
    mock.requests()
        .iter()
        .filter(|request| request.method == "PUT")
        .count()
}

#[tokio::test]
async fn onboarding_audit_failure_prevents_unrecorded_grants() {
    let Some(db) = database().await else {
        return;
    };
    sqlx::raw_sql("CREATE FUNCTION refuse_onboarding_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER refuse_onboarding_audit BEFORE INSERT ON operational_audit_log FOR EACH ROW EXECUTE FUNCTION refuse_onboarding_audit();")
        .execute(db.pool()).await.unwrap();
    let mock = discord(Arc::new(Mutex::new(State::default()))).await;
    runtime(&db, &mock, &[(MEMBER_ROLE_KEY, "1001")])
        .handle(
            OnboardingJob::Interaction(Box::new(press(5000))),
            1_790_781_200_000,
        )
        .await
        .unwrap();
    assert_eq!(
        puts(&mock),
        0,
        "an unavailable audit sink must refuse before Discord mutation"
    );
    assert!(reply(&mock).contains("couldn't confirm"));
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn onboarding_verify_concurrent_workers_use_live_roles_and_one_put() {
    let Some(db) = database().await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(State::default()))).await;
    let a = runtime(&db, &mock, &[(MEMBER_ROLE_KEY, "1001")]);
    let b = runtime(&db, &mock, &[(MEMBER_ROLE_KEY, "1001")]);
    let (first, second) = tokio::join!(
        a.handle(
            OnboardingJob::Interaction(Box::new(press(5001))),
            1_790_781_200_000
        ),
        b.handle(
            OnboardingJob::Interaction(Box::new(press(5002))),
            1_790_781_200_001
        )
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(puts(&mock), 1);
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM operational_audit_log ORDER BY action")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(actions, ["granted:verify", "verified:already"]);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn onboarding_verify_live_refusals_never_put_or_claim_verified() {
    let Some(db) = database().await else {
        return;
    };
    for (index, (role, state)) in [
        ("1002", State::default()),
        ("1003", State::default()),
        ("1004", State::default()),
        ("1005", State::default()),
        ("404", State::default()),
        (
            "1001",
            State {
                no_manage: true,
                ..State::default()
            },
        ),
        (
            "1001",
            State {
                pending: true,
                ..State::default()
            },
        ),
        (
            "1001",
            State {
                bot: true,
                ..State::default()
            },
        ),
        (
            "1001",
            State {
                absent: true,
                ..State::default()
            },
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mock = discord(Arc::new(Mutex::new(state))).await;
        runtime(&db, &mock, &[(MEMBER_ROLE_KEY, role)])
            .handle(
                OnboardingJob::Interaction(Box::new(press(5100 + index as u64))),
                1_790_781_200_000 + index as i64,
            )
            .await
            .unwrap();
        assert_eq!(puts(&mock), 0, "role {role}");
        assert!(reply(&mock).contains("couldn't confirm"), "role {role}");
        mock.shutdown().await;
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn onboarding_session_dry_run_and_wrong_component_surface_cannot_grant() {
    let Some(db) = database().await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(State::default()))).await;
    let live = runtime(&db, &mock, &[(MEMBER_ROLE_KEY, "1001")]);
    for (kind, component, guild) in [(5, 2, "22"), (3, 3, "22"), (3, 2, "23")] {
        let mut value = serde_json::to_value(press(5201)).unwrap();
        value["type"] = json!(kind);
        value["guild_id"] = json!(guild);
        if kind == 5 {
            value["data"] = json!({"custom_id":VERIFY_CUSTOM_ID,"components":[]});
        } else {
            value["data"]["component_type"] = json!(component);
        }
        let interaction = serde_json::from_value(value).unwrap();
        assert!(!live.accepts_interaction(&interaction));
    }
    assert!(mock.requests().is_empty());
    for overrides in [
        vec![(MEMBER_ROLE_KEY, "1001"), ("TWO_ONBOARDING_DRY_RUN", "1")],
        vec![
            (MEMBER_ROLE_KEY, "1001"),
            ("TWO_ONBOARDING_MODE", "session"),
            ("DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID", "10"),
            ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID", "11"),
        ],
    ] {
        runtime(&db, &mock, &overrides)
            .handle(
                OnboardingJob::Interaction(Box::new(press(5202))),
                1_790_781_200_001,
            )
            .await
            .unwrap();
        assert!(reply(&mock).contains("paused"));
    }
    assert_eq!(puts(&mock), 0);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn onboarding_admin_checks_fresh_permissions_then_saves_clears_and_posts_panel() {
    let Some(db) = database().await else {
        return;
    };
    let state = Arc::new(Mutex::new(State::default()));
    let mock = discord(Arc::clone(&state)).await;
    let live = runtime(&db, &mock, &[]);
    let join = admin(
        5301,
        "join-roles",
        json!([{"name":"roles","type":3,"value":"1001"}]),
    );
    live.configure(&join).await.unwrap();
    assert!(reply(&mock).contains("Manage Server"));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM guild_settings")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    state.lock().unwrap().admin = true;
    live.configure(&join).await.unwrap();
    assert!(reply(&mock).contains("saved"));
    let value: serde_json::Value =
        sqlx::query_scalar("SELECT value FROM guild_settings WHERE key=$1")
            .bind(JOIN_ROLES_KEY)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(value, json!(["1001"]));
    for raw in [
        "1001,1001",
        "22",
        "01",
        "1004",
        "1002",
        "1003",
        "1005",
        "404",
    ] {
        live.configure(&admin(
            5302,
            "join-roles",
            json!([{"name":"roles","type":3,"value":raw}]),
        ))
        .await
        .unwrap();
        assert!(!reply(&mock).contains("saved"));
    }
    live.configure(&admin(5303, "panel", json!([])))
        .await
        .unwrap();
    assert!(reply(&mock).contains("Set a member role"));
    live.configure(&admin(
        5304,
        "member-role",
        json!([{"name":"role","type":8,"value":"1001"}]),
    ))
    .await
    .unwrap();
    live.configure(&admin(5305, "panel", json!([])))
        .await
        .unwrap();
    assert!(reply(&mock).contains("panel posted"));
    let post = mock
        .requests()
        .into_iter()
        .find(|request| request.method == "POST" && request.path.ends_with("/messages"))
        .unwrap();
    let panel: serde_json::Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(
        panel["components"][0]["components"][0]["custom_id"],
        json!(VERIFY_CUSTOM_ID)
    );
    for name in ["clear-join", "clear-member"] {
        live.configure(&admin(5306, name, json!([]))).await.unwrap();
        assert!(reply(&mock).contains("cleared"));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM guild_settings")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(puts(&mock), 0, "configuring never grants");
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn onboarding_join_grants_without_a_welcome_channel_and_rejections_are_audited() {
    let Some(db) = database().await else {
        return;
    };
    let state = Arc::new(Mutex::new(State::default()));
    let mock = discord(Arc::clone(&state)).await;
    let live = runtime(&db, &mock, &[(JOIN_ROLES_KEY, "1001")]);
    let job = || OnboardingJob::Welcome {
        guild_id: 22,
        member_id: 44,
        bot: false,
        pending: false,
        trigger: MembershipTrigger::Joined { pending: false },
        roles: vec![],
    };
    live.handle(job(), 1_790_781_200_000).await.unwrap();
    assert_eq!(puts(&mock), 1);
    state.lock().unwrap().held.clear();
    state.lock().unwrap().reject_put = true;
    assert!(live.handle(job(), 1_790_781_200_001).await.is_err());
    let causes: Vec<String> =
        sqlx::query_scalar("SELECT action FROM operational_audit_log ORDER BY action")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(causes, ["failed:discord_unconfirmed", "granted:join"]);
    mock.shutdown().await;
    db.close().await.unwrap();
}
