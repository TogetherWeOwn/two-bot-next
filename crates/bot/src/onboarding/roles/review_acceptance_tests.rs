//! Independent production-boundary regressions for onboarding roles.
//! Only Discord is doubled; command routing, settings, audit and jobs are real.
//! Builder must preserve these assertions while repairing the runtime.
#![cfg(test)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_model::application::interaction::Interaction;
use two_bot_core::onboarding::MembershipTrigger;
use two_bot_core::RouterGates;
use two_bot_cutover::settings::SettingsStore;
use two_bot_discord::ActionExecutor;
use two_bot_testsupport::TestDatabase;

use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::onboarding::{OnboardingJob, OnboardingRuntime};

const JOIN_KEY: &str = "TWO_ONBOARDING_JOIN_ROLES";
const MEMBER_KEY: &str = "TWO_ONBOARDING_MEMBER_ROLE";
const NOW: i64 = 1_790_781_200_000;

fn roles() -> Value {
    json!([
        {"id":"22","permissions":"3072","managed":false,"position":0},
        {"id":"1001","permissions":"0","managed":false,"position":1},
        {"id":"1006","permissions":"268435456","managed":false,"position":5},
        {"id":"1009","permissions":"32","managed":false,"position":2}
    ])
}

fn channel(id: &str, kind: u8, overwrites: Value) -> Value {
    json!({"id":id,"guild_id":"22","type":kind,"permission_overwrites":overwrites})
}

struct State {
    roles: Value,
    channels: Value,
    channels_status: u16,
    channel_status: u16,
    identity: String,
    admin: bool,
    held: HashSet<String>,
    reject_put: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            roles: roles(),
            channels: json!([
                channel("10", 0, json!([])),
                channel("11", 2, json!([])),
                channel("12", 0, json!([]))
            ]),
            channels_status: 200,
            channel_status: 200,
            identity: "999".to_owned(),
            admin: false,
            held: HashSet::new(),
            reject_put: false,
        }
    }
}

async fn database() -> Option<TestDatabase> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").ok().or_else(|| {
        std::env::var("GITHUB_ACTIONS")
            .is_ok()
            .then(|| "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci".to_owned())
    })?;
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("owned disposable test database"),
    )
}

async fn discord(state: Arc<Mutex<State>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let mut state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/users/@me") => ScriptedResponse::json(
                200,
                json!({"id":state.identity,"username":"bot","discriminator":"0","bot":true}),
            ),
            ("GET", "/guilds/22") => {
                ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"}))
            }
            ("GET", "/guilds/22/roles") => ScriptedResponse::json(200, state.roles.clone()),
            ("GET", "/guilds/22/channels") => {
                ScriptedResponse::json(state.channels_status, state.channels.clone())
            }
            ("GET", "/guilds/22/members/999") => ScriptedResponse::json(
                200,
                json!({"user":{"id":state.identity,"username":"bot","bot":true},
                       "roles":["1006"],"pending":false}),
            ),
            ("GET", "/guilds/22/members/44") => {
                let mut held: Vec<_> = state.held.iter().cloned().collect();
                if state.admin {
                    held.push("1009".to_owned());
                }
                ScriptedResponse::json(
                    200,
                    json!({"user":{"id":"44","username":"member","discriminator":"0","bot":false},
                           "roles":held,"pending":false}),
                )
            }
            ("GET", path) if path.starts_with("/channels/") => {
                let id = path.strip_prefix("/channels/").unwrap();
                let found = state
                    .channels
                    .as_array()
                    .and_then(|channels| channels.iter().find(|channel| channel["id"] == id));
                match found {
                    Some(channel) => ScriptedResponse::json(state.channel_status, channel.clone()),
                    None => ScriptedResponse::status(404),
                }
            }
            ("PUT", path) if path.starts_with("/guilds/22/members/44/roles/") => {
                if state.reject_put {
                    return ScriptedResponse::status(403);
                }
                state
                    .held
                    .insert(path.rsplit('/').next().unwrap().to_owned());
                ScriptedResponse::status(204)
            }
            ("POST", "/channels/12/messages") => {
                ScriptedResponse::json(201, json!({"id":"99","channel_id":"12"}))
            }
            ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
            ("PATCH", path) if path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            _ => panic!(
                "unexpected review fixture request: {} {path}",
                request.method
            ),
        }
    })
    .await
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("onboarding-review-fixture".into(), Some(mock.origin())).unwrap()
}

fn runtime(pool: &PgPool, mock: &MockRest, extras: &[(&str, &str)]) -> OnboardingRuntime {
    let mut vars = HashMap::from([
        ("TWO_ONBOARDING_MODE".into(), "legacy".into()),
        ("TWO_ONBOARDING_DRY_RUN".into(), "0".into()),
    ]);
    vars.extend(
        extras
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    OnboardingRuntime::new(pool.clone(), executor(mock), &vars, 22, 999).unwrap()
}

fn commands(pool: &PgPool, mock: &MockRest) -> Arc<CommandRuntime> {
    CommandRuntime::new(
        pool.clone(),
        executor(mock),
        router_with_commands(RouterGates {
            configured_guild: Some(22),
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        }),
        22,
        false,
    )
}

fn welcome(trigger: MembershipTrigger) -> OnboardingJob {
    OnboardingJob::Welcome {
        guild_id: 22,
        member_id: 44,
        bot: false,
        pending: false,
        trigger,
        roles: vec![],
    }
}

fn interaction() -> Interaction {
    let id = rand::random_range(1..=u64::MAX);
    serde_json::from_value(json!({
        "application_id":"1111","authorizing_integration_owners":{"0":"22"},
        "id":id.to_string(),"token":format!("review-fixture-{id}"),
        "type":3,"version":1,"guild_id":"22","channel":{"id":"12","type":0},
        "member":{"user":{"id":"44","username":"member","discriminator":"0","bot":false},
                  "roles":[],"permissions":"8","deaf":false,"mute":false,"flags":0},
        "data":{"custom_id":"two:onboarding:verify","component_type":2}
    }))
    .unwrap()
}

fn slash(name: &str, options: Value) -> Interaction {
    let mut value = serde_json::to_value(interaction()).unwrap();
    value["type"] = json!(2);
    value["data"] = json!({"id":"77","name":"onboarding","type":1,
                          "options":[{"name":name,"type":1,"options":options}]});
    serde_json::from_value(value).unwrap()
}

fn role_puts(requests: &[RestRequest]) -> usize {
    requests
        .iter()
        .filter(|r| r.method == "PUT" && r.path.contains("/roles/"))
        .count()
}

fn message_posts(requests: &[RestRequest]) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/messages"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn completed_reply(requests: &[RestRequest], interaction: &Interaction) -> Value {
    let callback = format!(
        "/api/v10/interactions/{}/{}/callback",
        interaction.id, interaction.token
    );
    let initial: Vec<_> = requests.iter().filter(|r| r.path == callback).collect();
    assert_eq!(initial.len(), 1, "one initial acknowledgement");
    let body: Value = serde_json::from_slice(&initial[0].body).unwrap();
    assert_eq!(body["type"], 5, "the real boundary defers before work");
    assert_eq!(body["data"]["flags"], 64, "the defer is ephemeral");
    let patch = format!(
        "/api/v10/webhooks/1111/{}/messages/@original",
        interaction.token
    );
    let edits: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path == patch)
        .collect();
    assert_eq!(
        edits.len(),
        1,
        "every accepted defer must be completed exactly once"
    );
    assert!(initial[0].received_at <= edits[0].received_at);
    serde_json::from_slice(&edits[0].body).unwrap()
}

fn assert_error_reply(requests: &[RestRequest], interaction: &Interaction) {
    let reply = completed_reply(requests, interaction);
    let content = reply["content"].as_str().unwrap();
    assert!(
        content.contains("couldn't confirm"),
        "honest operational failure: {content}"
    );
    assert!(!content.contains("now verified"));
    assert!(!content.contains("already verified"));
    assert_eq!(role_puts(requests), 0, "a failed preflight must not grant");
}

async fn audits(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT COALESCE(action,''), COALESCE(target_id,''), metadata_json
         FROM operational_audit_log WHERE event_kind='onboarding_role_grant'
         ORDER BY created_at, entry_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn assert_refusal(rows: &[(String, String, String)]) {
    assert!(
        !rows.is_empty(),
        "the evaluated role needs an explicit audit cause"
    );
    assert!(
        rows.iter().all(|(action, target, _)| {
            target == "44"
                && (action.starts_with("refused:")
                    || action.starts_with("failed:")
                    || action.starts_with("skipped:"))
        }),
        "no grant or attempted-only audit may claim success: {rows:?}"
    );
}

async fn reject_audit(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_review_audit() RETURNS trigger LANGUAGE plpgsql AS
         $$ BEGIN RAISE EXCEPTION 'synthetic audit unavailable'; END $$;
         CREATE TRIGGER refuse_review_audit BEFORE INSERT ON operational_audit_log
         FOR EACH ROW WHEN (NEW.event_kind = 'onboarding_role_grant')
         EXECUTE FUNCTION refuse_review_audit();",
    )
    .execute(pool)
    .await
    .unwrap();
}

// Removing channel-overwrite privilege validation permits both grants and config.
#[tokio::test]
async fn onboarding_review_channel_manage_channels_blocks_join_and_verify() {
    let Some(db) = database().await else {
        return;
    };
    let state = State {
        channels: json!([channel(
            "12",
            0,
            json!([
                {"id":"1001","type":0,"allow":"16","deny":"0"}
            ])
        )]),
        ..State::default()
    };
    let state = Arc::new(Mutex::new(state));
    let mock = discord(Arc::clone(&state)).await;
    let live = runtime(
        db.pool(),
        &mock,
        &[(JOIN_KEY, "1001"), (MEMBER_KEY, "1001")],
    );
    let joined = live
        .handle(welcome(MembershipTrigger::Joined { pending: false }), NOW)
        .await;
    // Exercise Verify with a live unheld role independently of join's outcome.
    state.lock().unwrap().held.clear();
    let press = interaction();
    let verified = live
        .handle(OnboardingJob::Interaction(Box::new(press.clone())), NOW + 1)
        .await;
    let requests = mock.requests();
    let rows = audits(db.pool()).await;
    mock.shutdown().await;
    db.close().await.unwrap();
    assert_eq!(
        role_puts(&requests),
        0,
        "guild permissions 0 does not make channel Manage Channels safe"
    );
    assert_refusal(&rows);
    assert_eq!(
        rows.len(),
        2,
        "both join and verify evaluate and audit the refusal"
    );
    assert!(
        joined.is_ok(),
        "unsafe optional role must not kill the welcome worker"
    );
    assert!(verified.is_ok());
    assert_error_reply(&requests, &press);
}

#[tokio::test]
async fn onboarding_review_channel_privilege_is_rejected_by_real_admin_without_storage() {
    let Some(db) = database().await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(State {
        admin: true,
        channels: json!([channel(
            "12",
            0,
            json!([
                {"id":"1001","type":0,"allow":"16","deny":"0"}
            ])
        )]),
        ..State::default()
    })))
    .await;
    let live = commands(db.pool(), &mock);
    let join = slash(
        "join-roles",
        json!([{"name":"roles","type":3,"value":"1001"}]),
    );
    let member = slash(
        "member-role",
        json!([{"name":"role","type":8,"value":"1001"}]),
    );
    live.on_interaction(&join).await;
    live.on_interaction(&member).await;
    let count: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM guild_settings), (SELECT count(*) FROM guild_settings_audit)",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.unwrap();
    assert_eq!(
        count,
        (0, 0),
        "privileged role config must store neither value nor settings audit"
    );
    for command in [&join, &member] {
        let reply = completed_reply(&requests, command);
        assert!(!reply["content"].as_str().unwrap().contains("saved"));
        assert!(reply["content"].as_str().unwrap().contains("role"));
    }
    assert_eq!(role_puts(&requests), 0);
    assert!(message_posts(&requests).is_empty());
}

// Ordinary member-channel access is the point of verification, not a privilege.
#[tokio::test]
async fn onboarding_review_view_and_send_overwrite_remains_grantable() {
    let Some(db) = database().await else {
        return;
    };
    let state = Arc::new(Mutex::new(State {
        channels: json!([channel(
            "12",
            0,
            json!([
                {"id":"22","type":0,"allow":"0","deny":"3072"},
                {"id":"1001","type":0,"allow":"3072","deny":"0"}
            ])
        )]),
        ..State::default()
    }));
    let mock = discord(Arc::clone(&state)).await;
    let live = runtime(
        db.pool(),
        &mock,
        &[(JOIN_KEY, "1001"), (MEMBER_KEY, "1001")],
    );
    let joined = live
        .handle(welcome(MembershipTrigger::Joined { pending: false }), NOW)
        .await;
    // Model a role removed between join and Verify so BOTH paths must grant.
    state.lock().unwrap().held.clear();
    let press = interaction();
    let verified = live
        .handle(OnboardingJob::Interaction(Box::new(press.clone())), NOW + 1)
        .await;
    let requests = mock.requests();
    let rows = audits(db.pool()).await;
    mock.shutdown().await;
    db.close().await.unwrap();
    assert!(joined.is_ok());
    assert!(verified.is_ok());
    assert_eq!(
        role_puts(&requests),
        2,
        "both join and Verify may unlock ordinary member channels"
    );
    assert_eq!(
        completed_reply(&requests, &press)["content"],
        "You are now verified. Welcome!"
    );
    assert_eq!(
        rows.iter().map(|r| r.0.as_str()).collect::<HashSet<_>>(),
        HashSet::from(["granted:join", "granted:verify"])
    );
}

#[tokio::test]
async fn onboarding_review_missing_or_malformed_channel_evidence_never_authorizes() {
    let Some(db) = database().await else {
        return;
    };
    let mut observations = Vec::new();
    for (label, status, channels) in [
        ("unavailable", 403, json!({"message":"unavailable"})),
        ("not an array", 200, json!({"channels":[]})),
        (
            "missing overwrites",
            200,
            json!([{"id":"12","guild_id":"22","type":0}]),
        ),
        (
            "malformed overwrite",
            200,
            json!([channel(
                "12",
                0,
                json!([
                    {"id":"1001","type":0,"allow":"not-a-mask","deny":"0"}
                ])
            )]),
        ),
    ] {
        let mock = discord(Arc::new(Mutex::new(State {
            channels_status: status,
            channels,
            ..State::default()
        })))
        .await;
        let press = interaction();
        let result = runtime(db.pool(), &mock, &[(MEMBER_KEY, "1001")])
            .handle(OnboardingJob::Interaction(Box::new(press.clone())), NOW)
            .await;
        observations.push((label, result, press, mock.requests()));
        mock.shutdown().await;
    }
    let rows = audits(db.pool()).await;
    db.close().await.unwrap();
    for (label, result, press, requests) in observations {
        assert_eq!(role_puts(&requests), 0, "{label}");
        assert!(
            result.is_ok(),
            "{label}: refusal must complete the interaction"
        );
        assert_error_reply(&requests, &press);
    }
    assert_refusal(&rows);
    assert_eq!(rows.len(), 4);
}

// Returning early from join_roles failure loses a healthy welcome and kills work.
#[tokio::test]
async fn onboarding_review_rejected_join_role_still_welcomes_once_and_is_terminal() {
    let Some(db) = database().await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(State {
        reject_put: true,
        ..State::default()
    })))
    .await;
    let live = runtime(
        db.pool(),
        &mock,
        &[(JOIN_KEY, "1001"), ("DISCORD_LANDING_CHANNEL_IDS", "12")],
    );
    let first = live
        .handle(welcome(MembershipTrigger::Joined { pending: false }), NOW)
        .await;
    let redelivery = live
        .handle(
            welcome(MembershipTrigger::Joined { pending: false }),
            NOW + 1,
        )
        .await;
    let requests = mock.requests();
    let rows = audits(db.pool()).await;
    mock.shutdown().await;
    db.close().await.unwrap();
    let posts = message_posts(&requests);
    assert_eq!(
        posts.len(),
        1,
        "role PUT 403 must neither suppress nor duplicate the eligible welcome"
    );
    assert!(
        posts[0].to_string().contains("44"),
        "welcome targets the eligible member"
    );
    assert!(
        first.is_ok() && redelivery.is_ok(),
        "optional role failure is a terminal recoverable outcome"
    );
    assert_eq!(
        role_puts(&requests),
        2,
        "one rejected role request per delivered job, never an HTTP retry"
    );
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|r| r.0 == "failed:discord_unconfirmed" && r.1 == "44"));
}

#[tokio::test]
async fn onboarding_review_join_audit_failure_never_writes_unrecorded_roles_or_suppresses_welcome()
{
    let Some(db) = database().await else {
        return;
    };
    reject_audit(db.pool()).await;
    let mock = discord(Arc::new(Mutex::new(State::default()))).await;
    let result = runtime(
        db.pool(),
        &mock,
        &[(JOIN_KEY, "1001"), ("DISCORD_LANDING_CHANNEL_IDS", "12")],
    )
    .handle(welcome(MembershipTrigger::Joined { pending: false }), NOW)
    .await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.unwrap();
    assert_eq!(
        role_puts(&requests),
        0,
        "audit failure precedes any role write"
    );
    assert_eq!(
        message_posts(&requests).len(),
        1,
        "optional-role audit failure is independent of welcome"
    );
    assert!(
        result.is_ok(),
        "the essential queue must survive an optional-feature failure"
    );
}

async fn verify_audit_failure(extras: &[(&str, &str)]) {
    let Some(db) = database().await else {
        return;
    };
    reject_audit(db.pool()).await;
    let mock = discord(Arc::new(Mutex::new(State::default()))).await;
    let press = interaction();
    let result = runtime(db.pool(), &mock, extras)
        .handle(OnboardingJob::Interaction(Box::new(press.clone())), NOW)
        .await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.unwrap();
    assert_error_reply(&requests, &press);
    assert!(
        result.is_ok(),
        "successful error edit terminates the job, not the worker"
    );
}

// These errors occur before grant_roles; a catch around grant_roles is too late.
#[tokio::test]
async fn onboarding_review_unconfigured_verify_audit_failure_completes_defer() {
    verify_audit_failure(&[]).await;
}

#[tokio::test]
async fn onboarding_review_dry_run_verify_audit_failure_completes_defer() {
    verify_audit_failure(&[(MEMBER_KEY, "1001"), ("TWO_ONBOARDING_DRY_RUN", "1")]).await;
}

#[tokio::test]
async fn onboarding_review_settings_failure_after_defer_completes_error_reply() {
    let Some(db) = database().await else {
        return;
    };
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::json(200, json!({"id":"98"})),
    )
    .await;
    let live = runtime(db.pool(), &mock, &[(MEMBER_KEY, "1001")]);
    let press = interaction();
    // The first config snapshot succeeds. Hold the callback response, then make
    // the NEXT settings read fail; no sleeps, races or production fault seam.
    let (result, ()) = tokio::join!(
        live.handle(OnboardingJob::Interaction(Box::new(press.clone())), NOW),
        async {
            gate.wait_for_request().await;
            sqlx::query("ALTER TABLE guild_settings RENAME TO review_hidden_settings")
                .execute(db.pool())
                .await
                .unwrap();
            gate.release();
        }
    );
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.unwrap();
    assert_error_reply(&requests, &press);
    assert!(
        result.is_ok(),
        "a settings failure after acknowledged defer must be terminal"
    );
}

// Removing either the owner-map binding or admin_command call loses real effects.
#[tokio::test]
async fn onboarding_review_real_slash_saves_member_join_clears_and_posts_public_verify_panel() {
    let Some(db) = database().await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(State {
        admin: true,
        ..State::default()
    })))
    .await;
    let live = commands(db.pool(), &mock);
    let mut interactions = Vec::new();
    for (name, options, key, expected) in [
        (
            "join-roles",
            json!([{"name":"roles","type":3,"value":"1001"}]),
            JOIN_KEY,
            Some(json!(["1001"])),
        ),
        (
            "member-role",
            json!([{"name":"role","type":8,"value":"1001"}]),
            MEMBER_KEY,
            Some(json!("1001")),
        ),
    ] {
        let command = slash(name, options);
        live.on_interaction(&command).await;
        let actual: Option<Value> =
            sqlx::query_scalar("SELECT value FROM guild_settings WHERE key=$1")
                .bind(key)
                .fetch_optional(db.pool())
                .await
                .unwrap();
        interactions.push((command, actual == expected, "saved"));
    }
    let panel = slash("panel", json!([]));
    live.on_interaction(&panel).await;
    interactions.push((panel, true, "panel posted"));
    for (name, key) in [("clear-join", JOIN_KEY), ("clear-member", MEMBER_KEY)] {
        let command = slash(name, json!([]));
        live.on_interaction(&command).await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM guild_settings WHERE key=$1")
            .bind(key)
            .fetch_one(db.pool())
            .await
            .unwrap();
        interactions.push((command, count == 0, "cleared"));
    }
    let audit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM guild_settings_audit")
        .fetch_one(db.pool())
        .await
        .unwrap();
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.unwrap();
    assert_eq!(
        audit_count, 4,
        "two saved values and two clears cross the real store"
    );
    for (command, stored_correctly, word) in interactions {
        assert!(stored_correctly, "slash must mutate the expected value");
        let reply = completed_reply(&requests, &command);
        assert!(reply["content"].as_str().unwrap().contains(word));
        let callback_index = requests
            .iter()
            .position(|r| r.path.contains(&format!("/interactions/{}/", command.id)))
            .unwrap();
        assert_eq!(
            requests[callback_index + 1].path,
            "/api/v10/users/@me",
            "real admin handler resolves identity after defer"
        );
    }
    let posts = message_posts(&requests);
    assert_eq!(
        posts.len(),
        1,
        "one public panel, separate from ephemeral command reply"
    );
    assert!(requests
        .iter()
        .any(|r| r.method == "POST" && r.path == "/api/v10/channels/12/messages"));
    assert_eq!(
        posts[0]["components"][0]["components"][0]["custom_id"],
        "two:onboarding:verify"
    );
    assert_eq!(
        posts[0]["components"][0]["components"][0]["label"],
        "Verify"
    );
    assert_ne!(posts[0]["flags"], 64, "public panel must not be ephemeral");
    assert_eq!(
        role_puts(&requests),
        0,
        "admin configuration never grants a member role"
    );
}

#[tokio::test]
async fn onboarding_review_real_slash_revoked_live_admin_cannot_mutate_settings_or_panel() {
    let Some(db) = database().await else {
        return;
    };
    SettingsStore::new(db.pool())
        .set("22", MEMBER_KEY, Some(json!("1001")), "fixture")
        .await
        .unwrap();
    let mock = discord(Arc::new(Mutex::new(State::default()))).await;
    let live = commands(db.pool(), &mock);
    let mut interactions = Vec::new();
    for (name, options) in [
        (
            "join-roles",
            json!([{"name":"roles","type":3,"value":"1001"}]),
        ),
        (
            "member-role",
            json!([{"name":"role","type":8,"value":"1001"}]),
        ),
        ("clear-member", json!([])),
        ("panel", json!([])),
    ] {
        let command = slash(name, options); // Captured ADMINISTRATOR; live role 1009 was revoked.
        live.on_interaction(&command).await;
        interactions.push(command);
    }
    let stored: Vec<(String, Value)> =
        sqlx::query_as("SELECT key,value FROM guild_settings ORDER BY key")
            .fetch_all(db.pool())
            .await
            .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM guild_settings_audit")
        .fetch_one(db.pool())
        .await
        .unwrap();
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.unwrap();
    assert_eq!(stored, vec![(MEMBER_KEY.to_owned(), json!("1001"))]);
    assert_eq!(
        count, 1,
        "only fixture setup is audited; revoked actor makes no change"
    );
    assert!(message_posts(&requests).is_empty());
    assert_eq!(role_puts(&requests), 0);
    for command in interactions {
        let reply = completed_reply(&requests, &command);
        assert!(reply["content"].as_str().unwrap().contains("Manage Server"));
    }
}

// Unconditional admission in panel authorization fails each independent deny.
#[tokio::test]
async fn onboarding_review_panel_requires_actor_and_bot_view_and_send() {
    let Some(db) = database().await else {
        return;
    };
    SettingsStore::new(db.pool())
        .set("22", MEMBER_KEY, Some(json!("1001")), "fixture")
        .await
        .unwrap();
    let mut observations = Vec::new();
    for (label, member, denied) in [
        ("actor ViewChannel", "44", "1024"),
        ("actor SendMessages", "44", "2048"),
        ("bot ViewChannel", "999", "1024"),
        ("bot SendMessages", "999", "2048"),
    ] {
        let mock = discord(Arc::new(Mutex::new(State {
            admin: true,
            channels: json!([channel(
                "12",
                0,
                json!([
                    {"id":member,"type":1,"allow":"0","deny":denied}
                ])
            )]),
            ..State::default()
        })))
        .await;
        let command = slash("panel", json!([]));
        commands(db.pool(), &mock).on_interaction(&command).await;
        observations.push((label, command, mock.requests()));
        mock.shutdown().await;
    }
    db.close().await.unwrap();
    for (label, command, requests) in observations {
        assert!(
            message_posts(&requests).is_empty(),
            "{label}: zero public message POSTs"
        );
        let reply = completed_reply(&requests, &command);
        let content = reply["content"].as_str().unwrap();
        assert!(!content.contains("panel posted"), "{label}");
        assert!(
            content.contains("channel") || content.contains("access"),
            "{label}: actionable refusal"
        );
        assert_eq!(role_puts(&requests), 0);
    }
}

#[tokio::test]
async fn onboarding_review_panel_refuses_inaccessible_and_unsupported_channels() {
    let Some(db) = database().await else {
        return;
    };
    SettingsStore::new(db.pool())
        .set("22", MEMBER_KEY, Some(json!("1001")), "fixture")
        .await
        .unwrap();
    let mut observations = Vec::new();
    for (label, status, kind) in [
        ("inaccessible", 403, 0),
        ("voice", 200, 2),
        ("category", 200, 4),
    ] {
        let mock = discord(Arc::new(Mutex::new(State {
            admin: true,
            channel_status: status,
            channels: json!([channel("12", kind, json!([]))]),
            ..State::default()
        })))
        .await;
        let command = slash("panel", json!([]));
        commands(db.pool(), &mock).on_interaction(&command).await;
        observations.push((label, command, mock.requests()));
        mock.shutdown().await;
    }
    db.close().await.unwrap();
    for (label, command, requests) in observations {
        assert!(message_posts(&requests).is_empty(), "{label}");
        assert!(!completed_reply(&requests, &command)["content"]
            .as_str()
            .unwrap()
            .contains("panel posted"));
    }
}

// Deleting the join-specific Session guard must cause a role PUT here.
#[tokio::test]
async fn onboarding_review_session_join_and_gate_cleared_never_write_roles() {
    let Some(db) = database().await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(State::default()))).await;
    let live = runtime(
        db.pool(),
        &mock,
        &[
            (JOIN_KEY, "1001"),
            ("TWO_ONBOARDING_MODE", "session"),
            ("DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID", "10"),
            ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID", "11"),
        ],
    );
    let joined = live
        .handle(welcome(MembershipTrigger::Joined { pending: false }), NOW)
        .await;
    let cleared = live
        .handle(welcome(MembershipTrigger::GateCleared), NOW + 1)
        .await;
    let requests = mock.requests();
    let rows = audits(db.pool()).await;
    mock.shutdown().await;
    db.close().await.unwrap();
    assert!(joined.is_ok() && cleared.is_ok());
    assert_eq!(
        role_puts(&requests),
        0,
        "Session is no-write even when dry_run=false and roles are grantable"
    );
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|r| r.0 == "skipped:session_mode" && r.1 == "44"));
}

// Each case is eligible and grantable except for ONE malformed policy field.
// Removing role-snapshot validation admits the otherwise ordinary target role.
#[tokio::test]
async fn onboarding_review_malformed_live_roles_refuse_before_any_grant() {
    let Some(db) = database().await else {
        return;
    };
    let mut cases = Vec::new();
    for (label, field, bad) in [
        ("negative position", "position", json!(-1)),
        ("zero id", "id", json!("0")),
        ("noncanonical id", "id", json!("01001")),
        ("nonnumeric id", "id", json!("role")),
        ("overflow id", "id", json!("18446744073709551616")),
        ("negative mask", "permissions", json!("-1")),
        ("malformed mask", "permissions", json!("NaN")),
        (
            "overflow mask",
            "permissions",
            json!("18446744073709551616"),
        ),
    ] {
        let mut snapshot = roles();
        // Corrupt an unrelated role, not the configured id: a missing-target
        // check alone must not make the ID/mask validation test pass.
        snapshot[3][field] = bad;
        cases.push((label, snapshot));
    }
    for (label, field, bad) in [
        (
            "duplicate id with conflicting managed",
            "managed",
            json!(true),
        ),
        (
            "duplicate id with conflicting permissions",
            "permissions",
            json!("8"),
        ),
    ] {
        let mut snapshot = roles();
        let mut duplicate = snapshot[1].clone();
        duplicate[field] = bad;
        snapshot.as_array_mut().unwrap().push(duplicate);
        cases.push((label, snapshot));
    }
    let mut no_everyone = roles();
    no_everyone.as_array_mut().unwrap().remove(0);
    cases.push(("missing everyone", no_everyone));
    let mut observations = Vec::new();
    for (label, roles) in cases {
        let mock = discord(Arc::new(Mutex::new(State {
            roles,
            ..State::default()
        })))
        .await;
        let press = interaction();
        let result = runtime(db.pool(), &mock, &[(MEMBER_KEY, "1001")])
            .handle(OnboardingJob::Interaction(Box::new(press.clone())), NOW)
            .await;
        observations.push((label, result, press, mock.requests()));
        mock.shutdown().await;
    }
    let rows = audits(db.pool()).await;
    db.close().await.unwrap();
    for (label, result, press, requests) in observations {
        assert_eq!(role_puts(&requests), 0, "{label}");
        assert!(
            result.is_ok(),
            "{label}: reply failure boundary must be terminal"
        );
        assert_error_reply(&requests, &press);
    }
    assert_eq!(rows.len(), 11);
    assert_refusal(&rows);
}

#[tokio::test]
async fn onboarding_review_mismatched_live_bot_identity_cannot_authorize() {
    let Some(db) = database().await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(State {
        identity: "998".to_owned(),
        ..State::default()
    })))
    .await;
    let press = interaction();
    let result = runtime(db.pool(), &mock, &[(MEMBER_KEY, "1001")])
        .handle(OnboardingJob::Interaction(Box::new(press.clone())), NOW)
        .await;
    let requests = mock.requests();
    let rows = audits(db.pool()).await;
    mock.shutdown().await;
    db.close().await.unwrap();
    assert_eq!(
        role_puts(&requests),
        0,
        "member response for another bot is not our authority"
    );
    assert_refusal(&rows);
    assert_error_reply(&requests, &press);
    assert!(result.is_ok());
}
