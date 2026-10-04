//! Real binary + scripted Discord + disposable Postgres acceptance.

#[allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod common;

use std::process::Output;
use std::time::Duration;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use tokio::process::Command;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ChannelClaim, ChannelModerationStore, InteractionRouter, RouterGates};
use two_bot_discord::{register_channel_handlers, ActionExecutor, ChannelModerationRuntime};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "111111111111111111";
const CHANNEL: &str = "222222222222222222";
const OPERATOR: &str = "333333333333333333";
const TIME: &str = "2026-10-04T00:00:00.000Z";

async fn cli(args: &[&str], url: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("TWO_DATABASE_TLS", "local-only")
        .args(args)
        .kill_on_drop(true);
    if let Some(url) = url {
        command.env("TWO_DATABASE_URL", url);
    }
    tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .expect("CLI must exit without starting the gateway")
        .unwrap()
}

fn args(key: &str) -> Vec<&str> {
    vec![
        "moderation",
        "release-channel",
        "--guild",
        GUILD,
        "--channel",
        CHANNEL,
        "--claim-key",
        key,
    ]
}

fn confirm<'a>(key: &'a str, generation: &'a str) -> Vec<&'a str> {
    let mut args = args(key);
    args.extend([
        "--confirm-release",
        "--expected-generation",
        generation,
        "--operator",
        OPERATOR,
        "--reason",
        "workers quiesced; REST settled; channel reconciled",
    ]);
    args
}

fn report(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::str::from_utf8(&output.stdout).unwrap();
    serde_json::from_str(text.lines().next().unwrap()).unwrap()
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

fn interaction(id: u64, action: &str) -> Interaction {
    let permissions = ((1u64 << 4) | (1 << 13)).to_string();
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "444444444444444444", "type": 2,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0, "permissions": permissions},
        "data": {"id": "555555555555555555", "type": 1, "name": action,
                 "options": [{"name": "reason", "type": 3, "value": "fixture reconciliation"}]},
        "member": {"user": {"id": OPERATOR, "username": "fixture-member", "discriminator": "0", "avatar": null},
                   "roles": [], "flags": 0, "deaf": false, "mute": false, "permissions": permissions},
        "token": "synthetic-interaction-token", "version": 1,
        "authorizing_integration_owners": {"0": GUILD}, "entitlements": []
    })).unwrap()
}

#[tokio::test]
async fn help_and_invalid_confirmation_are_side_effect_free_and_redact_diagnostics() {
    for help_args in [
        vec!["moderation", "release-channel", "--help"],
        vec!["moderation", "--help"],
        vec!["--help"],
    ] {
        let help = cli(&help_args, None).await;
        assert!(help.status.success());
        assert!(String::from_utf8_lossy(&help.stdout).contains("--confirm-release"));
    }
    let base = args("123");
    let mut missing = base.clone();
    missing.push("--confirm-release");
    let missing = cli(&missing, None).await;
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--expected-generation"));
    for extra in [
        vec!["--guild", GUILD],
        vec!["--confirm-release", "--confirm-release"],
        vec!["--operator", OPERATOR],
        vec!["--allow-owed"],
        vec!["--claim-token", "fixture-private-value"],
    ] {
        let mut invalid = base.clone();
        invalid.extend(extra);
        let output = cli(&invalid, None).await;
        assert_eq!(output.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-private-value"));
    }
    let failure = cli(&base, Some("invalid-database-url:fixture-private-value")).await;
    assert_eq!(failure.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&failure.stderr).contains("fixture-private-value"));
}

#[tokio::test]
async fn cli_releases_a_503_wedge_then_unlock_restores_the_surviving_seed() {
    let db = database().await;
    let url = format!("postgres://agent_test:@agent-testdb:5432/{}", db.name());
    let store = ChannelModerationStore::from_pool(db.pool().clone());
    let mock = MockRest::start(vec![
        ScriptedResponse::json(200, json!({"permission_overwrites": [{"id": GUILD, "type": 0, "allow": "3072", "deny": "8192"}]})),
        ScriptedResponse::status(503),
        ScriptedResponse::status(204),
    ], ScriptedResponse::status(500)).await;
    let runtime = ChannelModerationRuntime::new(
        store.clone(),
        ActionExecutor::with_proxy("synthetic-test-token".to_owned(), Some(mock.origin())).unwrap(),
    );
    let mut router = InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
        moderation: true,
        scorecard: false,
        automations: false,
        announcements: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    register_channel_handlers(&mut router);
    let lock = interaction(700, "lockdown");
    assert_eq!(
        runtime
            .execute(&router, &lock)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "in_progress"
    );
    let seed = store.get_lockdown(CHANNEL).await.unwrap().unwrap();
    assert_eq!(
        runtime
            .execute(&router, &interaction(701, "unlock"))
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "in_progress"
    );
    assert_eq!(mock.requests().len(), 2);
    let dry = cli(&args("700"), Some(&url)).await;
    let inspected = report(&dry);
    assert!(String::from_utf8_lossy(&dry.stdout).contains("INSPECTION ONLY: no changes"));
    assert!(store
        .inspect_channel_lane(GUILD, CHANNEL, "700")
        .await
        .unwrap()
        .is_some());
    let release = cli(
        &confirm("700", inspected["expected_generation"].as_str().unwrap()),
        Some(&url),
    )
    .await;
    assert_eq!(report(&release), inspected);
    assert!(String::from_utf8_lossy(&release.stdout).contains("\"released\":true"));
    assert_eq!(store.get_lockdown(CHANNEL).await.unwrap(), Some(seed));
    let old = runtime.execute(&router, &lock).await.unwrap().unwrap();
    assert!(old.replayed);
    assert_eq!(old.outcome, "operator_released");
    assert_eq!(
        mock.requests().len(),
        2,
        "old delivery must not repeat uncertain PUT"
    );
    assert_eq!(
        runtime
            .execute(&router, &interaction(702, "unlock"))
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "unlocked"
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].method, "PUT");
    let restored: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(restored["allow"], "3072");
    assert_eq!(restored["deny"], "8192");
    assert!(store.get_lockdown(CHANNEL).await.unwrap().is_none());
    let audit: (String, String) = sqlx::query_as("SELECT actor_id, metadata_json FROM moderation_audit WHERE action = 'moderation.channel_lane_release' AND idempotency_key = '700'")
        .fetch_one(db.pool()).await.unwrap();
    assert_eq!(audit.0, OPERATOR);
    let metadata: Value = serde_json::from_str(&audit.1).unwrap();
    assert_eq!(metadata["previous"], inspected);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn cli_refuses_stale_inspections_and_done_claims() {
    let db = database().await;
    let url = format!("postgres://agent_test:@agent-testdb:5432/{}", db.name());
    let store = ChannelModerationStore::from_pool(db.pool().clone());
    for key in ["stale", "done"] {
        let ChannelClaim::Claimed { ticket } = store
            .claim(GUILD, key, "moderation.lockdown", "hash", TIME)
            .await
            .unwrap()
        else {
            panic!("claim");
        };
        assert!(store.claim_channel(&ticket, CHANNEL).await.unwrap());
        let before = report(&cli(&args(key), Some(&url)).await);
        let generation = before["expected_generation"].as_str().unwrap();
        if key == "stale" {
            assert!(store.release(&ticket).await.unwrap());
            let ChannelClaim::Claimed { ticket: new } = store
                .claim(GUILD, key, "moderation.lockdown", "hash", TIME)
                .await
                .unwrap()
            else {
                panic!("new claim");
            };
            assert!(store.claim_channel(&new, CHANNEL).await.unwrap());
            let refused = cli(&confirm(key, generation), Some(&url)).await;
            assert_eq!(refused.status.code(), Some(1));
            assert!(store
                .inspect_channel_lane(GUILD, CHANNEL, key)
                .await
                .unwrap()
                .unwrap()
                .releasable());
            assert!(store.release(&new).await.unwrap());
        } else {
            assert!(store
                .complete(&ticket, "locked_down", "{}", TIME)
                .await
                .unwrap());
            let done = report(&cli(&args(key), Some(&url)).await);
            for fingerprint in [generation, done["expected_generation"].as_str().unwrap()] {
                let refused = cli(&confirm(key, fingerprint), Some(&url)).await;
                assert_eq!(refused.status.code(), Some(1));
            }
            assert!(store
                .inspect_channel_lane(GUILD, CHANNEL, key)
                .await
                .unwrap()
                .is_some());
        }
    }
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_audit")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(audits, 0);
    db.close().await.unwrap();
}
