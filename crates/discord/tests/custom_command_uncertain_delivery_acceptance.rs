#![cfg(feature = "db")]

//! Acceptance: a proven completion rejection audits a definitive failed run,
//! while an ambiguous completion (transport/5xx/429/timeout, including an
//! accepted PATCH whose response is lost) is `DeliveryUnknown`, is never
//! resent, and never creates a definitive `delivery_failed` run fact.
//!
//! Contract, using the existing error vocabulary (`Rejected`/guard refusals
//! are safe pre-mutation; timeouts, transport/5xx, and 429 are not):
//!
//! - Explicit rejection (definitive 4xx on the completion PATCH): `Delivery`,
//!   one failed `command.run` fact with reason `delivery_failed`, exactly one
//!   PATCH (no retry). Success still records `(ok, None)` with the rendered
//!   template.
//! - Uncertain completion: `DeliveryUnknown`, exactly one PATCH (never
//!   resend an uncertain completion), and no definitive failed fact: either
//!   no `custom:run:<interaction_id>` row at all (like an ambiguous defer)
//!   or a row with outcome `unknown` and a bounded reason. A later explicit
//!   retry of the same interaction then records only its own outcome.
//! - Controls pin the preserved surface: absent/disabled rows stay silent,
//!   audit rows carry no template or rendered content, and a definitive
//!   failure keeps first-write-wins on explicit re-entry.
//!
//! RED on baseline: `complete()` maps every transport failure to `Delivery`
//! and records `delivery_failed`, so the uncertain tests fail on the variant
//! and on the definitive fact. Loopback REST only; the database is
//! agent-testdb or the credential-free CI Postgres service.
//!
//! NOTE (builder): this target carries `#[ignore]` database tests, so the
//! combined PR must route it explicitly in `check.yml`
//! (`--test custom_command_uncertain_delivery_acceptance -- --include-ignored`
//! in the ignored runtime lane) to satisfy the test-inventory gate.

#[allow(dead_code)]
mod common;

use std::sync::Arc;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{InteractionRouter, RouterGates};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime},
    ActionExecutor,
};

const MIGRATION: &str = include_str!("../../cutover/migrations/0130_custom_commands.sql");

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        automations: true,
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

fn runtime(pool: sqlx::PgPool, mock: &MockRest) -> CustomCommandRuntime {
    let mut router = InteractionRouter::new(gates());
    CustomCommandRuntime::register(&mut router);
    CustomCommandRuntime::new(
        pool,
        Arc::new(router),
        ActionExecutor::with_proxy("uncertain-delivery-fixture".to_owned(), Some(mock.origin()))
            .expect("mock executor"),
        1111,
    )
}

fn slash(id: u64, name: &str) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": "uncertain-delivery-fixture", "version": 1,
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

fn test_options() -> PgConnectOptions {
    let ci_service = std::env::var("CI").as_deref() == Ok("true")
        && std::env::var("TWO_CUSTOM_COMMAND_TEST_CI").as_deref() == Ok("1");
    PgConnectOptions::new()
        .host(if ci_service {
            "127.0.0.1"
        } else {
            "agent-testdb"
        })
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test")
        .options([("statement_timeout", "5000ms")])
}

async fn test_pool() -> sqlx::PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::raw_sql(
                    "CREATE TEMP TABLE custom_command_scope (id INT); SET search_path = pg_temp;",
                )
                .execute(&mut *conn)
                .await?;
                sqlx::raw_sql(MIGRATION).execute(&mut *conn).await?;
                Ok(())
            })
        })
        .connect_with(test_options())
        .await
        .expect("authorized test DB")
}

async fn seed_faq(pool: &sqlx::PgPool, template: &str) {
    two_bot_core::custom_command_service::put(
        pool,
        true,
        "2222",
        "3333",
        &two_bot_core::custom_commands::PutCommandInput {
            name: "faq".to_owned(),
            description: "FAQ".to_owned(),
            template: template.to_owned(),
            text_trigger: None,
        },
        "seed",
        &two_bot_core::now_iso(),
    )
    .await
    .expect("seed faq");
}

async fn run_fact(pool: &sqlx::PgPool, id: u64) -> Option<(String, Option<String>)> {
    sqlx::query_as("SELECT outcome, reason FROM automation_audit_log WHERE id = $1")
        .bind(format!("custom:run:{id}"))
        .fetch_optional(pool)
        .await
        .expect("audit read")
}

/// An uncertain completion leaves no definitive failed fact: either no row
/// (like an ambiguous defer) or an `unknown` row with a bounded reason.
fn assert_no_definitive_failed_fact(fact: Option<(String, Option<String>)>, id: u64) {
    match fact {
        None => {}
        Some((outcome, reason)) if outcome == "unknown" => {
            if let Some(reason) = reason {
                assert!(
                    reason.len() <= 64,
                    "bounded uncertainty diagnostic, got {} chars",
                    reason.len()
                );
            }
        }
        other => panic!("uncertain completion must not create a definitive failed fact for custom:run:{id}, got {other:?}"),
    }
}

fn patch_bodies(mock: &MockRest) -> Vec<Value> {
    mock.requests()
        .iter()
        .filter(|request| request.method == "PATCH")
        .map(|request| serde_json::from_slice(&request.body).expect("patch body"))
        .collect()
}

fn ok_receipt() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"id": "9000"}))
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn explicit_completion_rejection_audits_failed_run_without_retry() {
    let pool = test_pool().await;
    seed_faq(&pool, "hello").await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool.clone(), &mock);
    match runtime
        .handle_interaction(&slash(400, "faq"), Some("Test guild"))
        .await
    {
        Err(CustomCommandError::Delivery) => {}
        other => panic!("definitive 403 completion is Delivery, got {other:?}"),
    }
    assert_eq!(mock.requests().len(), 2, "defer plus one PATCH, no retry");
    assert_eq!(patch_bodies(&mock).len(), 1);
    assert_eq!(
        run_fact(&pool, 400).await,
        Some(("failed".to_owned(), Some("delivery_failed".to_owned()))),
        "proven rejection keeps its definitive failed fact"
    );
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn uncertain_completion_500_is_unknown_without_failed_fact_or_resend() {
    let pool = test_pool().await;
    seed_faq(&pool, "hello").await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(500)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool.clone(), &mock);
    match runtime
        .handle_interaction(&slash(401, "faq"), Some("Test guild"))
        .await
    {
        Err(CustomCommandError::DeliveryUnknown) => {}
        other => panic!("ambiguous 500 completion is DeliveryUnknown, got {other:?}"),
    }
    assert_eq!(
        mock.requests().len(),
        2,
        "uncertain completion is never resent"
    );
    assert_eq!(patch_bodies(&mock).len(), 1);
    assert_no_definitive_failed_fact(run_fact(&pool, 401).await, 401);
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn uncertain_completion_accepted_patch_with_lost_response_is_unknown() {
    let pool = test_pool().await;
    seed_faq(&pool, "hello").await;
    // The server accepts the PATCH, but its receipt arrives after the shared
    // executor's five-second deadline. Acceptance is unknown: uncertain, not
    // proven failed.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ok_receipt().delayed(std::time::Duration::from_secs(6)),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool.clone(), &mock);
    match runtime
        .handle_interaction(&slash(402, "faq"), Some("Test guild"))
        .await
    {
        Err(CustomCommandError::DeliveryUnknown) => {}
        other => panic!("lost completion receipt is DeliveryUnknown, got {other:?}"),
    }
    assert_eq!(
        patch_bodies(&mock).len(),
        1,
        "an accepted edit is never repeated after response loss"
    );
    assert_no_definitive_failed_fact(run_fact(&pool, 402).await, 402);
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn uncertain_completion_rate_limit_is_unknown_without_failed_fact() {
    let pool = test_pool().await;
    seed_faq(&pool, "hello").await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::rate_limited(0.01, "0.01"),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool.clone(), &mock);
    match runtime
        .handle_interaction(&slash(403, "faq"), Some("Test guild"))
        .await
    {
        Err(CustomCommandError::DeliveryUnknown) => {}
        other => panic!("rate-limited completion is DeliveryUnknown, got {other:?}"),
    }
    assert_eq!(patch_bodies(&mock).len(), 1, "no automatic resend");
    assert_no_definitive_failed_fact(run_fact(&pool, 403).await, 403);
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn successful_completion_audit_is_preserved() {
    let pool = test_pool().await;
    seed_faq(&pool, "Hi {user} {username} in {server} {channel}").await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ok_receipt()],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool.clone(), &mock);
    match runtime
        .handle_interaction(&slash(404, "faq"), Some("Test guild"))
        .await
    {
        Ok(true) => {}
        other => panic!("successful run is owned, got {other:?}"),
    }
    let patches = patch_bodies(&mock);
    assert_eq!(patches.len(), 1);
    assert_eq!(
        patches[0]["content"],
        "Hi <@3333> tester in Test guild <#4444>"
    );
    assert_eq!(patches[0]["allowed_mentions"]["parse"], json!([]));
    assert_eq!(
        run_fact(&pool, 404).await,
        Some(("ok".to_owned(), None)),
        "success keeps its (ok, None) fact"
    );
    mock.shutdown().await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn definitive_failure_preserves_first_fact_on_explicit_retry() {
    let pool = test_pool().await;
    seed_faq(&pool, "hello").await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime1 = runtime(pool.clone(), &mock);
    assert!(matches!(
        runtime1
            .handle_interaction(&slash(405, "faq"), Some("Test guild"))
            .await,
        Err(CustomCommandError::Delivery)
    ));
    mock.shutdown().await;
    // Explicit re-entry still attempts delivery, but the first-write-wins
    // fact keeps the original definitive failure.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ok_receipt()],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime2 = runtime(pool.clone(), &mock);
    match runtime2
        .handle_interaction(&slash(405, "faq"), Some("Test guild"))
        .await
    {
        Ok(true) => {}
        other => panic!("explicit retry attempts delivery, got {other:?}"),
    }
    mock.shutdown().await;
    assert_eq!(
        run_fact(&pool, 405).await,
        Some(("failed".to_owned(), Some("delivery_failed".to_owned()))),
        "first-write-wins preserves the definitive failure"
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn uncertain_completion_retry_records_only_its_own_outcome() {
    let pool = test_pool().await;
    seed_faq(&pool, "hello").await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(500)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime1 = runtime(pool.clone(), &mock);
    // The variant is pinned by the 500 test above; here the attempt must only
    // fail without recording, so the retry below owns the fact.
    assert!(runtime1
        .handle_interaction(&slash(406, "faq"), Some("Test guild"))
        .await
        .is_err());
    mock.shutdown().await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ok_receipt()],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime2 = runtime(pool.clone(), &mock);
    match runtime2
        .handle_interaction(&slash(406, "faq"), Some("Test guild"))
        .await
    {
        Ok(true) => {}
        other => panic!("retry after uncertainty attempts delivery, got {other:?}"),
    }
    mock.shutdown().await;
    assert_eq!(
        run_fact(&pool, 406).await,
        Some(("ok".to_owned(), None)),
        "an uncertain attempt must not poison its retry with a failed fact"
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn absent_and_disabled_commands_stay_silent() {
    let pool = test_pool().await;
    seed_faq(&pool, "hello").await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(pool.clone(), &mock);
    match runtime
        .handle_interaction(&slash(407, "nope"), Some("Test guild"))
        .await
    {
        Ok(false) => {}
        other => panic!("absent command stays silent and unowned, got {other:?}"),
    }
    sqlx::query("UPDATE automation_commands SET enabled = FALSE WHERE name = 'faq'")
        .execute(&pool)
        .await
        .expect("disable faq");
    match runtime
        .handle_interaction(&slash(408, "faq"), Some("Test guild"))
        .await
    {
        Ok(false) => {}
        other => panic!("disabled command stays silent and unowned, got {other:?}"),
    }
    assert!(mock.requests().is_empty(), "definitive miss sends nothing");
    mock.shutdown().await;
    pool.close().await;
}

#[derive(sqlx::FromRow)]
struct AuditRow {
    guild_id: String,
    actor_id: Option<String>,
    action: String,
    target_key: Option<String>,
    outcome: String,
    reason: Option<String>,
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn completion_audit_rows_carry_no_template_or_rendered_content() {
    let pool = test_pool().await;
    seed_faq(&pool, "audit marker xyzzy {user}").await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ok_receipt(),
            ScriptedResponse::status(204),
            ScriptedResponse::status(403),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool.clone(), &mock);
    assert!(runtime
        .handle_interaction(&slash(409, "faq"), Some("Test guild"))
        .await
        .expect("success path"));
    assert!(runtime
        .handle_interaction(&slash(410, "faq"), Some("Test guild"))
        .await
        .is_err());
    mock.shutdown().await;
    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT guild_id, actor_id, action, target_key, outcome, reason \
             FROM automation_audit_log WHERE action = 'command.run' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("audit rows");
    assert_eq!(rows.len(), 2);
    for row in &rows {
        let joined = [
            row.guild_id.as_str(),
            row.actor_id.as_deref().unwrap_or_default(),
            row.action.as_str(),
            row.target_key.as_deref().unwrap_or_default(),
            row.outcome.as_str(),
            row.reason.as_deref().unwrap_or_default(),
        ]
        .join(" ");
        assert!(
            !joined.contains("xyzzy"),
            "audit row stores no template content: {joined}"
        );
        assert!(
            !joined.contains("<@"),
            "audit row stores no rendered mention: {joined}"
        );
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn uncertain_completion_render_failed_still_audits_failed_run() {
    let pool = test_pool().await;
    seed_faq(&pool, "hello").await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(500)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool.clone(), &mock);
    // `None` guild name fails rendering (`rendered == None`, reason
    // `render_failed`), but the uncertain completion still audits.
    match runtime.handle_interaction(&slash(411, "faq"), None).await {
        Err(CustomCommandError::DeliveryUnknown) => {}
        other => panic!("render-failed uncertain completion is DeliveryUnknown, got {other:?}"),
    }
    assert_eq!(patch_bodies(&mock).len(), 1);
    assert_eq!(
        run_fact(&pool, 411).await,
        Some(("failed".to_owned(), Some("render_failed".to_owned()))),
        "render failure still audits its failed run"
    );
    mock.shutdown().await;
    pool.close().await;
}
