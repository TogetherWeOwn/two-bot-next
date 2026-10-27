#![cfg(test)]
//! Review hardening for the private admin goodbye preview.
//!
//! The frozen acceptance oracle pins the specified behavior; this module pins
//! the review findings without touching the frozen files: pre-admission
//! yield of the goodbye-preview slash under a saturated shared lane, and the
//! goodbye-specific unconfirmed-ACK guidance. It mirrors the welcome-preview
//! review harness with its own fixtures — never import from the frozen module.
//!
//! Offline only: closed lazy pools and loopback Discord, no database.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use twilight_model::application::interaction::Interaction;
use twilight_model::gateway::event::Event;
use two_bot_discord::ActionExecutor;

use crate::gateway::ensure_crypto_provider;
use crate::onboarding::{OnboardingJob, OnboardingRuntime};

const NOW: i64 = 1_790_780_400_000;

fn admin_member(user_id: &str, permissions: Option<&str>) -> Value {
    let mut member = json!({
        "roles": [], "deaf": false, "mute": false, "flags": 0,
        "user": {"id": user_id, "username": "admin", "discriminator": "0", "bot": false},
    });
    if let Some(bits) = permissions {
        member["permissions"] = Value::String(bits.to_owned());
    }
    member
}

/// A `/goodbye-preview` slash from admin member 44 in guild 22: the exact
/// Manage Server bit, zero options.
fn preview() -> Interaction {
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": "22"},
        "id": "333", "token": "mock-callback", "type": 2, "version": 1, "guild_id": "22",
        "member": admin_member("44", Some("32")),
        "data": {"id": "444", "name": "goodbye-preview", "type": 1, "options": []},
    }))
    .expect("valid goodbye-preview slash fixture")
}

fn job(interaction: Interaction) -> OnboardingJob {
    OnboardingJob::Interaction(Box::new(interaction))
}

fn vars(mode: &str, dry_run: bool) -> HashMap<String, String> {
    HashMap::from([
        ("DISCORD_GUILD_ID".into(), "22".into()),
        ("TWO_ONBOARDING_MODE".into(), mode.into()),
        (
            "TWO_ONBOARDING_DRY_RUN".into(),
            if dry_run { "1" } else { "0" }.into(),
        ),
        ("DISCORD_LANDING_CHANNEL_IDS".into(), "12".into()),
        ("DISCORD_GOODBYE_CHANNEL_IDS".into(), "13".into()),
        ("DISCORD_ANCHOR_WELCOME_CHANNEL_ID".into(), "14".into()),
        (
            "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID".into(),
            "10".into(),
        ),
        ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID".into(), "11".into()),
    ])
}

fn runtime(pool: &PgPool, mock: &MockRest, vars: &HashMap<String, String>) -> OnboardingRuntime {
    ensure_crypto_provider();
    let executor =
        ActionExecutor::with_proxy("onboarding-test-only-token".into(), Some(mock.origin()))
            .unwrap();
    OnboardingRuntime::new(pool.clone(), executor, vars, 22, 999).unwrap()
}

fn closed_pool() -> PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/postgres")
        .expect("offline lazy pool")
}

fn callbacks(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path.ends_with("/callback"))
        .collect()
}

fn replies(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "PATCH")
        .collect()
}

#[derive(Default)]
struct PreviewDiscordState {
    reject_callback: bool,
}

/// Strict responder: the defer callback and the completing edit only. Every
/// other route panics, proving the preview performs no member resolution,
/// channel post, role write or invite/voice/audit traffic.
async fn preview_discord(state: Arc<Mutex<PreviewDiscordState>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("POST", path) if path.ends_with("/callback") => {
                if state.reject_callback {
                    return ScriptedResponse::status(403);
                }
                ScriptedResponse::status(204)
            }
            ("PATCH", path) if path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            _ => panic!(
                "preview performs no Discord read or write besides defer+edit: {} {}",
                request.method, path
            ),
        }
    })
    .await
}

fn shared_preview_runtime(
    pool: &PgPool,
    mock: &MockRest,
) -> Arc<crate::command_runtime::CommandRuntime> {
    ensure_crypto_provider();
    let executor = ActionExecutor::with_proxy(
        "goodbye-preview-test-only-token".into(),
        Some(mock.origin()),
    )
    .expect("loopback-only shared executor");
    let gates = two_bot_core::RouterGates {
        configured_guild: Some(22),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: true,
        session_picker: false,
    };
    crate::command_runtime::CommandRuntime::new(
        pool.clone(),
        executor,
        crate::command_runtime::router_with_commands(gates),
        22,
        false,
    )
}

// A saturated shared command lane must yield ownership before it can send a
// competing busy ACK. Occupancy is deterministic; no database or timers fill it.
#[tokio::test]
async fn goodbye_preview_shared_dispatch_yields_before_busy_admission() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/postgres")
        .expect("offline lazy pool");
    pool.close().await;
    let busy_seen = Arc::new(tokio::sync::Notify::new());
    let signal = Arc::clone(&busy_seen);
    let mock = MockRest::with_responder(move |request: &RestRequest| {
        assert_eq!(request.method, "POST");
        assert!(request.path.ends_with("/callback"));
        signal.notify_one();
        ScriptedResponse::status(204)
    })
    .await;
    let shared = shared_preview_runtime(&pool, &mock);
    let _dispatch_guard = shared.dispatch_guard();
    let _slots: Vec<_> = (0..crate::interaction_admission::PER_USER_IN_FLIGHT)
        .map(|_| {
            shared
                .acquire_user_slot_for_test(44)
                .expect("hold actor slot")
        })
        .collect();
    let mut invocation = preview();
    // The shared test constructor pins this application; a mismatched fixture
    // would silently bypass busy admission and make the oracle vacuous.
    invocation.application_id = twilight_model::id::Id::new(1111);
    let event = Event::InteractionCreate(Box::new(
        twilight_model::gateway::payload::incoming::InteractionCreate(invocation),
    ));
    assert!(
        shared.dispatch(&event),
        "the shared runtime yields goodbye-preview before its exhausted actor budget"
    );
    // The same actor's ordinary slash must still hit busy admission. This
    // positive control rules out globally skipping the shared admission gate.
    let mut ordinary = serde_json::to_value(preview()).unwrap();
    ordinary["application_id"] = json!("1111");
    ordinary["id"] = json!("335");
    ordinary["token"] = json!("mock-busy-control");
    ordinary["data"]["name"] = json!("help");
    let ordinary = serde_json::from_value(ordinary).expect("ordinary slash control");
    let ordinary = Event::InteractionCreate(Box::new(
        twilight_model::gateway::payload::incoming::InteractionCreate(ordinary),
    ));
    assert!(!shared.dispatch(&ordinary), "the ordinary slash is busy");
    tokio::time::timeout(Duration::from_secs(2), busy_seen.notified())
        .await
        .expect("the ordinary slash receives its busy callback");
    let requests = callbacks(&mock);
    assert_eq!(requests.len(), 1, "only the ordinary slash receives an ACK");
    assert!(requests[0].path.contains("/335/mock-busy-control/"));
    let reply: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64);
    assert_eq!(
        reply["data"]["content"],
        crate::interaction_admission::BUSY_REPLY
    );
}

// A failed preview ACK renders the preview-specific guidance (banner plus
// no-delivery wording), never the generic already-processed fallback.
// Reverting the branch to the generic reply must fail this.
#[tokio::test]
async fn goodbye_preview_unconfirmed_ack_states_no_delivery_attempted() {
    let pool = closed_pool();
    pool.close().await;
    let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState {
        reject_callback: true,
    })))
    .await;
    let runtime = runtime(&pool, &mock, &vars("legacy", false));
    runtime
        .handle(job(preview()), NOW)
        .await
        .expect("the unconfirmed preview guides privately");
    assert_eq!(callbacks(&mock).len(), 1, "the failed ACK is never retried");
    let edits = replies(&mock);
    assert_eq!(edits.len(), 1, "one guidance edit");
    let body: Value = serde_json::from_slice(&edits[0].body).unwrap();
    let content = body["content"].as_str().unwrap_or_default();
    let first = content.lines().next().unwrap_or_default();
    assert!(
        first.to_lowercase().contains("preview"),
        "the guidance keeps the preview banner, got {first:?}"
    );
    assert!(
        content.contains("No delivery was attempted"),
        "the guidance states no delivery was attempted, got {content:?}"
    );
    assert!(
        !content.contains("may already have been processed"),
        "no generic already-processed fallback, got {content:?}"
    );
}
