//! Protected review regressions: the shared dispatcher must yield before
//! admission, and onboarding must use the executor's registered identity.
//! Tests only; no database connection or live Discord.
#![cfg(test)]

use super::*;
use crate::onboarding::OnboardingRuntime;
use std::collections::HashMap;
use twilight_model::application::command::Command;
use two_bot_core::CommandDefinition;

const SHARED_INTERACTIONS: usize = 1;
const REOPEN_ID: u64 = 9001;
const HELP_ID: u64 = 9002;

fn registered(name: &str, id: u64) -> Command {
    let mut command = two_bot_discord::publish_commands(&[CommandDefinition::new(
        name,
        "Synthetic registered command.",
    )])
    .remove(0);
    command.id = Some(Id::new(id));
    command.application_id = Some(Id::new(1111));
    command.guild_id = Some(Id::new(GUILD));
    command
}

fn invocation(id: u64, incoming_name: &str) -> Interaction {
    let mut interaction = from_member(
        slash(
            incoming_name,
            Some(CHANNEL),
            vec![option("kind", CommandOptionValue::String("games".into()))],
        ),
        42,
        Permissions::empty(),
    );
    let Some(InteractionData::ApplicationCommand(command)) = interaction.data.as_mut() else {
        panic!("slash fixture");
    };
    command.id = Id::new(id);
    command.guild_id = Some(Id::new(GUILD));
    interaction
}

fn onboarding(pool: &PgPool, executor: ActionExecutor) -> OnboardingRuntime {
    executor
        .command_identities()
        .replace_guild(
            1111,
            GUILD,
            &[registered("reopen", REOPEN_ID), registered("help", HELP_ID)],
        )
        .unwrap();
    OnboardingRuntime::new(
        pool.clone(),
        executor,
        &HashMap::from([
            ("DISCORD_GUILD_ID".into(), GUILD.to_string()),
            ("TWO_ONBOARDING_MODE".into(), "legacy".into()),
            ("TWO_ONBOARDING_DRY_RUN".into(), "0".into()),
        ]),
        GUILD,
        999,
    )
    .unwrap()
}

async fn saturated_dispatch_yields(per_member: bool, remaining: bool) {
    let (mock, origin) = MockRest::start(vec![]).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_unused")
        .unwrap();
    let executor = executor_at(origin);
    let onboarding = onboarding(&pool, executor.clone());
    let runtime = CommandRuntime::new(
        pool,
        executor,
        router_with_commands(gates(false, false)),
        GUILD,
        false,
    );
    let _guard = runtime.dispatch_guard();
    let mut slots = Vec::new();
    if per_member {
        for _ in 0..3 {
            slots.push(runtime.acquire_user_slot_for_test(42).unwrap());
        }
        assert!(runtime.acquire_user_slot_for_test(42).is_none());
    } else {
        for _ in 0..16 {
            assert!(runtime.hold_lane_for_test(SHARED_INTERACTIONS, Duration::from_secs(30)));
        }
        assert_eq!(runtime.lane_in_flight(SHARED_INTERACTIONS), 16);
    }

    let interaction = invocation(REOPEN_ID, "reopen");
    assert!(onboarding.accepts_interaction(&interaction));
    let event = interaction_event(interaction.clone());
    if remaining {
        runtime.dispatch_remaining(&event);
    } else {
        runtime.dispatch(&event);
    }
    // Exercise the real ingress owner as well: the one defer must be its
    // ephemeral type-5 response, not the dispatcher's overload callback.
    assert!(
        onboarding
            .acknowledge(&interaction, tokio::time::Instant::now())
            .await
    );
    wait_for(
        || runtime.lane_in_flight(crate::command_runtime::LANE_BUSY) == 0,
        "any incorrectly competing busy callback to settle",
    )
    .await;
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        1,
        "shared admission must not race the onboarding-owned defer"
    );
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].path,
        "/api/v10/interactions/42/token-42/callback"
    );
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["type"], 5, "onboarding alone owns the defer");
    assert_eq!(body["data"]["flags"], 64, "the defer stays private");
    assert_eq!(
        runtime.dispatch_drops_total(SHARED_INTERACTIONS),
        0,
        "reopen never attempts admission into the shared lane"
    );
    assert_eq!(
        runtime.lane_in_flight(SHARED_INTERACTIONS),
        if per_member { 0 } else { 16 },
        "yielding consumes no shared interaction slot"
    );
    drop(slots);
    mock.shutdown().await;
}

// Moving the Reopen yield below admit_interaction creates a second callback
// at either boundary. Both public dispatch entrypoints must keep one owner.
#[tokio::test]
async fn reopen_dispatch_yields_before_three_per_member_slots() {
    saturated_dispatch_yields(true, false).await;
}

#[tokio::test]
async fn reopen_remaining_dispatch_yields_before_three_per_member_slots() {
    saturated_dispatch_yields(true, true).await;
}

#[tokio::test]
async fn reopen_dispatch_yields_before_sixteen_shared_slots() {
    saturated_dispatch_yields(false, false).await;
}

#[tokio::test]
async fn reopen_remaining_dispatch_yields_before_sixteen_shared_slots() {
    saturated_dispatch_yields(false, true).await;
}

async fn capture_registered_identity(id: u64, incoming_name: &str, expected: bool) {
    let (mock, origin) = MockRest::start(vec![]).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_unused")
        .unwrap();
    let runtime = onboarding(&pool, executor_at(origin));
    let interaction = invocation(id, incoming_name);
    let captured = runtime.accepts_interaction(&interaction);
    if captured {
        assert!(
            runtime
                .acknowledge(&interaction, tokio::time::Instant::now())
                .await
        );
    }
    assert_eq!(captured, expected, "registered ID, not incoming slash name");
    let requests = mock.requests();
    assert_eq!(requests.len(), usize::from(expected));
    if expected {
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["type"], 5);
        assert_eq!(body["data"]["flags"], 64);
    }
    mock.shutdown().await;
}

// Raw-name capture either loses a legitimate registered invocation or steals
// another command's acknowledgement. Unknown IDs in a hydrated scope fail shut.
#[tokio::test]
async fn reopen_registered_id_with_stale_help_name_is_captured_and_acked() {
    capture_registered_identity(REOPEN_ID, "help", true).await;
}

#[tokio::test]
async fn reopen_name_with_registered_help_id_is_not_captured_or_acked() {
    capture_registered_identity(HELP_ID, "reopen", false).await;
}

#[tokio::test]
async fn reopen_name_with_unknown_id_in_known_scope_is_not_captured_or_acked() {
    capture_registered_identity(9999, "reopen", false).await;
}
