//! Acceptance: one reply owner through the actual gateway custom-command
//! path. A slash lookup outage is owned with exactly one ephemeral
//! availability reply; foreign, gated, and disabled-surface inputs stay
//! silent and unowned. Offline: closed pools plus loopback REST, no database.
//!
//! The availability copy is pinned in
//! `crates/discord/tests/custom_command_lookup_outage_acceptance.rs`; this
//! module repeats the literal (unit and integration targets cannot share it)
//! and the builder must keep every copy identical.
//!
//! RED on baseline: the runtime returns before any callback, so the gateway
//! ownership test fails on the missing reply and the gated-outage test fails
//! on the owned error; the silence controls already pass.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use twilight_model::{application::interaction::Interaction, id::Id};
use two_bot_core::{InteractionRouter, RouterGates};
use two_bot_discord::{custom_commands::CustomCommandRuntime, ActionExecutor};

use crate::discord_test_common::{MockRest, ScriptedResponse};
use crate::gateway_commands::{GatewayCommandConfig, GatewayCommands};

/// Must stay identical to the pinned copy in the discord acceptance target.
const AVAILABILITY_REPLY: &str =
    "Custom commands are temporarily unavailable. Please try again shortly.";

fn gates(automations: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        automations,
        moderation: false,
        voice: false,
        voice_assistant: false,
        scorecard: false,
        announcements: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn slash(id: u64, name: &str) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": "gateway-outage-fixture", "version": 1,
        "guild_id": "2222", "channel": {"id": "4444", "type": 0, "name": "commands"},
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": [],
        "member": {
            "permissions": "0", "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": name, "type": 1, "options": []}
    }))
    .expect("valid Twilight interaction fixture")
}

async fn closed_pool() -> sqlx::PgPool {
    // Never connects: closed before use, so every lookup fails immediately.
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    pool.close().await;
    pool
}

fn commands(pool: sqlx::PgPool, mock: &MockRest, automations: bool) -> GatewayCommands {
    let vars = if automations {
        HashMap::from([("TWO_AUTOMATIONS".to_owned(), "1".to_owned())])
    } else {
        HashMap::new()
    };
    let config =
        GatewayCommandConfig::from_map_for_activation(2222, &vars, true).expect("command config");
    let mut router = InteractionRouter::new(gates(automations));
    CustomCommandRuntime::register(&mut router);
    GatewayCommands::for_test(
        pool,
        ActionExecutor::with_proxy("gateway-outage-fixture".to_owned(), Some(mock.origin()))
            .expect("mock executor"),
        config,
        Arc::new(router),
    )
}

#[tokio::test]
async fn gateway_lookup_failure_sends_single_availability_reply_and_owns_interaction() {
    let pool = closed_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let commands = commands(pool.clone(), &mock, true);
    assert!(
        commands.handle_interaction(&slash(500, "faq")).await,
        "lookup outage is owned: no fallback handler may reply"
    );
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        1,
        "one gateway reply: no double fallback, no retry"
    );
    assert_eq!(requests[0].method, "POST");
    assert!(
        requests[0].path.ends_with("/callback"),
        "immediate callback, got {}",
        requests[0].path
    );
    let body: Value = serde_json::from_slice(&requests[0].body).expect("callback body");
    assert_eq!(body["type"], 4, "immediate reply, never a defer");
    assert_eq!(body["data"]["flags"], 64, "ephemeral");
    assert_eq!(body["data"]["content"], AVAILABILITY_REPLY);
    assert_eq!(body["data"]["allowed_mentions"]["parse"], json!([]));
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
async fn gateway_foreign_and_gated_inputs_stay_silent() {
    let pool = closed_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let commands = commands(pool.clone(), &mock, true);
    let mut foreign_app = slash(510, "faq");
    foreign_app.application_id = Id::new(9999);
    let mut foreign_guild = slash(511, "faq");
    foreign_guild.guild_id = Some(Id::new(7777));
    let mut no_guild = slash(512, "faq");
    no_guild.guild_id = None;
    for (label, interaction) in [
        ("foreign app", foreign_app),
        ("foreign guild", foreign_guild),
        ("missing guild", no_guild),
        ("builtin name", slash(513, "rank")),
    ] {
        assert!(
            !commands.handle_interaction(&interaction).await,
            "{label} stays silent and unowned"
        );
    }
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
async fn gateway_gated_lookup_failure_stays_silent_when_automations_are_off() {
    let pool = closed_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let commands = commands(pool.clone(), &mock, false);
    assert!(
        !commands.handle_interaction(&slash(520, "faq")).await,
        "gated outage stays silent and unowned"
    );
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
    pool.close().await;
}
