#![cfg(feature = "db")]

//! Acceptance: a custom-command slash lookup outage gets one bounded,
//! fixed-text, ephemeral availability reply; gated inputs stay silent.
//!
//! Contract (slash surface only; prefix text triggers cannot be ephemeral and
//! keep their existing silent behavior):
//!
//! - In scope (configured guild, automations on, non-builtin, non-management
//!   slash) with a failed definition lookup — immediate SQL/pool error or a
//!   lookup pending past the 750 ms budget — the runtime answers the
//!   interaction exactly once with [`AVAILABILITY_REPLY`]: an immediate
//!   type-4 callback, ephemeral, mention-suppressed, generic fixed text. The
//!   call is owned (`Ok(true)`), so the gateway never falls through to a
//!   second handler. No cached or guessed definition executes: no defer, no
//!   PATCH edit, no registry PUT. The reply lands inside the existing
//!   receipt budget ([`RECEIPT_ADMISSION_BUDGET_MS`]), leaving margin below
//!   Discord's three-second acknowledgement deadline.
//! - Out of scope stays silent with no database or Discord I/O: foreign app
//!   or guild, missing guild, builtin names, non-slash payloads, and — even
//!   when the lookup itself fails — a disabled automations gate.
//! - Management permission refusals still precede any lookup.
//!
//! RED on baseline: the lookup error returns before any callback, so the
//! availability tests fail on the missing reply and the gated-outage test
//! fails on the error; the silence/permission controls already pass.

#[allow(dead_code)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use twilight_model::{application::interaction::Interaction, id::Id};
use two_bot_core::{InteractionRouter, RouterGates};
use two_bot_discord::{
    custom_commands::CustomCommandRuntime, executor::RECEIPT_ADMISSION_BUDGET_MS, ActionExecutor,
};

/// Pinned availability copy: generic fixed text, no command name, error, or
/// transport detail. The builder quotes this exact string in the reply and in
/// `docs/custom-commands.md`; a different copy needs a Test Author correction.
const AVAILABILITY_REPLY: &str =
    "Custom commands are temporarily unavailable. Please try again shortly.";

fn gates(enabled: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        automations: enabled,
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

fn runtime(pool: sqlx::PgPool, mock: &MockRest, enabled: bool) -> CustomCommandRuntime {
    let mut router = InteractionRouter::new(gates(enabled));
    CustomCommandRuntime::register(&mut router);
    CustomCommandRuntime::new(
        pool,
        Arc::new(router),
        ActionExecutor::with_proxy("lookup-outage-fixture".to_owned(), Some(mock.origin()))
            .expect("mock executor"),
        1111,
    )
}

fn slash(id: u64, name: &str, permissions: u64) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": "lookup-outage-fixture", "version": 1,
        "guild_id": "2222", "channel": {"id": "4444", "type": 0, "name": "commands"},
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": [],
        "member": {
            "permissions": permissions.to_string(), "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": name, "type": 1, "options": []}
    }))
    .expect("valid Twilight interaction fixture")
}

fn ping(id: u64) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 1,
        "token": "lookup-outage-fixture", "version": 1,
        "guild_id": "2222",
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": []
    }))
    .expect("valid Twilight ping fixture")
}

fn test_options() -> PgConnectOptions {
    PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test")
        .options([("statement_timeout", "5000ms")])
}

/// A pool that fails every lookup immediately: lazy, then closed before use.
async fn closed_pool() -> sqlx::PgPool {
    let pool = PgPoolOptions::new().connect_lazy_with(test_options());
    pool.close().await;
    pool
}

/// A pool whose first connection hangs in the Postgres handshake: the stub
/// accepts the TCP connection and then holds it silent, so the 750 ms lookup
/// timeout fires while the server side is still pending. Fully offline
/// (loopback); the caller aborts the stub task.
async fn stalling_pool() -> (sqlx::PgPool, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback stub");
    let port = listener.local_addr().expect("stub port").port();
    let stub = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(socket);
            });
        }
    });
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy(&format!(
            "postgres://agent_test@127.0.0.1:{port}/agent_test"
        ))
        .expect("lazy pool");
    (pool, stub)
}

/// The one availability callback: immediate, ephemeral, fixed copy, no leaks,
/// and the only request on the wire (no defer, edit, or registry publish).
fn assert_availability_reply(mock: &MockRest) {
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        1,
        "exactly one availability callback: no fallback, retry, or execution"
    );
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert!(
        request.path.ends_with("/callback"),
        "immediate callback, got {}",
        request.path
    );
    let body: Value = serde_json::from_slice(&request.body).expect("callback body is JSON");
    assert_eq!(body["type"], 4, "immediate reply, never a defer");
    assert_eq!(body["data"]["flags"], 64, "ephemeral");
    assert_eq!(body["data"]["content"], AVAILABILITY_REPLY);
    assert_eq!(body["data"]["allowed_mentions"]["parse"], json!([]));
    let content = body["data"]["content"]
        .as_str()
        .expect("availability content");
    assert!(
        content.len() <= 200,
        "bounded fixed copy, got {} chars",
        content.len()
    );
    for leak in [
        "faq",
        "postgres",
        "SQL",
        "storage",
        "token",
        "lookup-outage-fixture",
    ] {
        assert!(
            !content.contains(leak),
            "generic copy leaks no command name or transport detail: {leak}"
        );
    }
}

fn assert_within_receipt_budget(elapsed: Duration) {
    assert!(
        elapsed < Duration::from_millis(RECEIPT_ADMISSION_BUDGET_MS) + Duration::from_millis(500),
        "reply lands inside the receipt budget plus CI headroom (Discord deadline is 3 s), took {elapsed:?}"
    );
}

#[tokio::test]
async fn immediate_lookup_failure_sends_one_ephemeral_availability_reply() {
    let pool = closed_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(pool.clone(), &mock, true);
    let started = tokio::time::Instant::now();
    let outcome = runtime
        .handle_interaction(&slash(300, "faq", 0), Some("Test guild"))
        .await;
    let elapsed = started.elapsed();
    match outcome {
        Ok(true) => {}
        other => panic!("lookup outage is owned with a reply, got {other:?}"),
    }
    assert_within_receipt_budget(elapsed);
    assert_availability_reply(&mock);
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
async fn pending_lookup_failure_sends_same_fixed_reply_within_receipt_budget() {
    let (pool, stub) = stalling_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(pool.clone(), &mock, true);
    let started = tokio::time::Instant::now();
    let outcome = runtime
        .handle_interaction(&slash(301, "faq", 0), Some("Test guild"))
        .await;
    let elapsed = started.elapsed();
    match outcome {
        Ok(true) => {}
        other => panic!("pending lookup outage is owned with a reply, got {other:?}"),
    }
    assert_within_receipt_budget(elapsed);
    // Same pinned literal as the immediate case: fixed text across modes.
    assert_availability_reply(&mock);
    stub.abort();
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
async fn foreign_and_gated_inputs_stay_silent_without_database_or_discord() {
    let pool = closed_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(pool.clone(), &mock, true);
    // A missed fence fails on the closed pool instead of leaking I/O.
    let mut foreign_app = slash(310, "faq", 0);
    foreign_app.application_id = Id::new(9999);
    let mut foreign_guild = slash(311, "faq", 0);
    foreign_guild.guild_id = Some(Id::new(7777));
    let mut no_guild = slash(312, "faq", 0);
    no_guild.guild_id = None;
    for (label, interaction) in [
        ("foreign app", foreign_app),
        ("foreign guild", foreign_guild),
        ("missing guild", no_guild),
        ("builtin name", slash(313, "rank", 0)),
        ("non-slash payload", ping(314)),
    ] {
        match runtime.handle_interaction(&interaction, None).await {
            Ok(false) => {}
            other => panic!("{label} stays silent and unowned, got {other:?}"),
        }
    }
    assert!(
        mock.requests().is_empty(),
        "gated inputs send nothing and need no database"
    );
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
async fn gated_lookup_failure_stays_silent_when_automations_are_off() {
    let pool = closed_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(pool.clone(), &mock, false);
    // The automations gate is known without the database: a gated outage is
    // silence, never an availability reply for a disabled surface.
    match runtime
        .handle_interaction(&slash(320, "faq", 0), Some("Test guild"))
        .await
    {
        Ok(false) => {}
        other => panic!("gated outage stays silent and unowned, got {other:?}"),
    }
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
async fn management_permission_refusals_precede_any_lookup() {
    let pool = closed_pool().await;
    for (enabled, permissions, expected) in [
        (true, 0, "Manage Server"),
        (false, 32, "Automations are disabled"),
    ] {
        let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
        let runtime = runtime(pool.clone(), &mock, enabled);
        match runtime
            .handle_interaction(&slash(330, "command-list", permissions), None)
            .await
        {
            Ok(true) => {}
            other => panic!("permission refusal is owned, got {other:?}"),
        }
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "{expected}");
        let body: Value = serde_json::from_slice(&requests[0].body).expect("refusal body");
        assert_eq!(body["type"], 4);
        assert_eq!(body["data"]["flags"], 64);
        assert!(
            body["data"]["content"]
                .as_str()
                .expect("refusal content")
                .contains(expected),
            "{expected}"
        );
        mock.shutdown().await;
    }
    pool.close().await;
}

#[test]
fn failure_and_unknown_semantics_are_documented() {
    let docs =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/custom-commands.md");
    let text = std::fs::read_to_string(&docs).expect("operator docs");
    assert!(
        text.contains("temporarily unavailable"),
        "docs/custom-commands.md names the availability reply"
    );
    assert!(
        text.contains("uncertain completion"),
        "docs/custom-commands.md states the uncertain-completion rule"
    );
}

#[tokio::test]
async fn slow_availability_callback_still_meets_receipt_deadline() {
    let (pool, stub) = stalling_pool().await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204).delayed(Duration::from_secs(4))],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool.clone(), &mock, true);
    let started = tokio::time::Instant::now();
    let outcome = runtime
        .handle_interaction(&slash(331, "faq", 0), Some("Test guild"))
        .await;
    let elapsed = started.elapsed();
    match outcome {
        Ok(true) => {}
        other => panic!("slow-callback outage is owned with a reply, got {other:?}"),
    }
    assert_within_receipt_budget(elapsed);
    // The timed-out 204 never lands, but the callback request assertions hold.
    assert_availability_reply(&mock);
    stub.abort();
    mock.shutdown().await;
    pool.close().await;
}
