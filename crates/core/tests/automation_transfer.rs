#![cfg(feature = "db")]

//! Opt-in database proof for automations import/export, restricted to
//! agent-testdb or the CI service container.
//! Run: cargo test -p two-bot-core --features db --test automation_transfer -- --ignored
//! No DATABASE_URL or inherited application credentials are consulted.

use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use two_bot_core::automation_transfer::{
    diff_import, export_document, max_import_entries, parse_import_document, ImportOutcome,
};
use two_bot_core::custom_command_service as service;
use two_bot_core::custom_command_store as store;
use two_bot_core::custom_commands::StoredCommand;

const MIGRATION: &str = include_str!("../../cutover/migrations/0130_custom_commands.sql");
const AT: &str = "2026-10-03T01:00:00Z";
const GUILD: &str = "1545644954272137297";
const ACTOR: &str = "900000000000000001";

/// One session behind the pool, so `search_path = pg_temp` survives the
/// service's own transactions. Everything, including DDL, vanishes when the
/// pool closes, even on an assertion failure.
async fn isolated_pool() -> PgPool {
    let ci_service = std::env::var("CI").as_deref() == Ok("true")
        && std::env::var("TWO_AUTOMATION_TRANSFER_TEST_CI").as_deref() == Ok("1");
    let options = PgConnectOptions::new()
        .host(if ci_service {
            "127.0.0.1"
        } else {
            "agent-testdb"
        })
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test")
        .options([("statement_timeout", "5000ms")]);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect_with(options)
        .await
        .expect("agent-testdb or CI service must be reachable");
    let mut setup = pool.begin().await.expect("begin setup");
    sqlx::query("CREATE TEMP TABLE automation_transfer_test_scope (id INTEGER)")
        .execute(&mut *setup)
        .await
        .expect("temp scope");
    // Session-level, not LOCAL: the service opens its own transactions on
    // this same single connection.
    sqlx::query("SET search_path = pg_temp")
        .execute(&mut *setup)
        .await
        .expect("search_path");
    sqlx::raw_sql(MIGRATION)
        .execute(&mut *setup)
        .await
        .expect("migrate");
    sqlx::raw_sql(MIGRATION)
        .execute(&mut *setup)
        .await
        .expect("migration re-runs clean");
    setup.commit().await.expect("commit setup");
    pool
}

fn parse(body: &serde_json::Value) -> two_bot_core::automation_transfer::ParsedImport {
    parse_import_document(body, max_import_entries()).expect("test body parses")
}

async fn audit_rows(pool: &PgPool) -> Vec<(String, String, String, Option<String>)> {
    sqlx::query_as("SELECT id, action, outcome, reason FROM automation_audit_log ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("read audits")
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn import_applies_mee6_payload_and_refuses_live_collisions() {
    let pool = isolated_pool().await;
    // Legacy `importMee6 preserves first text trigger` vectors.
    let parsed = parse(&json!([
        {"command": "faq", "response": "Read {server} rules"},
        {"command": "FAQ", "response": "second faq"},
        {"command": "welcome", "description": "says hi", "response": "hi {user}"},
        "",
    ]));
    let outcome = service::import(&pool, GUILD, ACTOR, &parsed, false, "import-one", AT)
        .await
        .expect("import applies");
    assert_eq!(
        outcome,
        ImportOutcome {
            imported: 3,
            skipped: 1,
            conflicts: vec!["faq".to_owned()],
        }
    );
    let faq = store::get_command(&pool, GUILD, "faq")
        .await
        .expect("read faq")
        .expect("faq stored");
    assert_eq!(faq.text_trigger.as_deref(), Some("!faq"));
    assert_eq!(
        store::get_command(&pool, GUILD, "faq-2")
            .await
            .expect("read faq-2")
            .and_then(|row| row.text_trigger),
        None,
        "later collisions import slash-only"
    );
    let welcome = store::get_command(&pool, GUILD, "welcome")
        .await
        .expect("read welcome")
        .expect("welcome stored");
    assert_eq!(welcome.template, "hi {user}");

    // A live admin row with different content is a collision, not an update.
    let live = StoredCommand {
        guild_id: GUILD.to_owned(),
        name: "live".to_owned(),
        description: "Live FAQ".to_owned(),
        template: "live answer".to_owned(),
        text_trigger: Some("!live".to_owned()),
        enabled: true,
    };
    store::put_command(&pool, &live, ACTOR, ACTOR, AT, AT)
        .await
        .expect("seed live");
    let changed = parse(&json!({
        "commands": [{"command": "live", "description": "MEE6 FAQ", "response": "imported answer"}],
    }));
    let outcome = service::import(&pool, GUILD, ACTOR, &changed, false, "import-live", AT)
        .await
        .expect("collision refusal is a result, not an error");
    assert_eq!(outcome.imported, 0);
    assert_eq!(outcome.skipped, 1);
    assert_eq!(outcome.conflicts, ["live"]);
    assert_eq!(
        store::get_command(&pool, GUILD, "live")
            .await
            .expect("read live")
            .expect("live stored")
            .template,
        "live answer",
        "refused import leaves the live row alone"
    );

    // Explicit overwrite with the capability updates the row.
    let forced = parse(&json!({
        "commands": [{"command": "live", "description": "MEE6 FAQ", "response": "imported answer"}],
        "overwrite": true,
    }));
    let outcome = service::import(&pool, GUILD, ACTOR, &forced, true, "import-force", AT)
        .await
        .expect("overwrite applies");
    assert_eq!(outcome.imported, 1);
    assert_eq!(
        store::get_command(&pool, GUILD, "live")
            .await
            .expect("read live")
            .expect("live stored")
            .template,
        "imported answer"
    );

    // Round trip: exporting the stored state and diffing it is a no-op.
    let rows = store::list_commands(&pool, GUILD).await.expect("list");
    let exported = serde_json::to_value(export_document(&rows)).expect("export serializes");
    let reparsed = parse(&exported);
    let diff = diff_import(
        &rows,
        &reparsed,
        &two_bot_core::custom_commands::builtin_command_names(),
        false,
    )
    .expect("diff plans");
    assert!(diff.to_create.is_empty() && diff.to_update.is_empty() && diff.rejected.is_empty());

    let facts = audit_rows(&pool).await;
    assert!(
        facts
            .iter()
            .any(|(id, action, outcome, _)| id == "import-one#summary"
                && action == "automations.import"
                && outcome == "imported:3,skipped:1,conflicts:1"),
        "summary audit recorded: {facts:?}"
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn import_skips_reserved_and_invalid_definitions() {
    let pool = isolated_pool().await;
    // Legacy `import capacity ignores invalid and reserved definitions`
    // vectors (without the artificial budget override).
    let parsed = parse(&json!([
        {"command": "rank", "response": "reserved"},
        {"command": "empty-template", "response": "{unsupported.placeholder}"},
        {"command": "valid", "response": "works"},
    ]));
    let outcome = service::import(&pool, GUILD, ACTOR, &parsed, false, "import-mixed", AT)
        .await
        .expect("import applies");
    assert_eq!(outcome.imported, 1);
    assert_eq!(outcome.skipped, 2);
    assert!(outcome.conflicts.is_empty());
    let names: Vec<String> = store::list_commands(&pool, GUILD)
        .await
        .expect("list")
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert_eq!(names, ["valid"]);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn import_capacity_overflow_writes_nothing() {
    let pool = isolated_pool().await;
    let seed = parse(&json!([{"command": "seed", "response": "s"}]));
    service::import(&pool, GUILD, ACTOR, &seed, false, "import-seed", AT)
        .await
        .expect("seed applies");
    // Exactly the budget fits the envelope cap, but the union with the stored
    // row exceeds it, so planning fails the whole import.
    let max = max_import_entries();
    let commands: Vec<serde_json::Value> = (0..max)
        .map(|i| json!({"command": format!("fresh-{i}"), "response": "t"}))
        .collect();
    let parsed = parse(&serde_json::to_value(commands).expect("to JSON"));
    let error = service::import(&pool, GUILD, ACTOR, &parsed, true, "import-full", AT)
        .await
        .expect_err("overflow is refused");
    assert!(
        matches!(error, service::ImportServiceError::OverCapacity(_)),
        "overflow error, got {error:?}"
    );
    let names: Vec<String> = store::list_commands(&pool, GUILD)
        .await
        .expect("list")
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert_eq!(names, ["seed"], "overflow is rejected before writes");
    let facts = audit_rows(&pool).await;
    assert!(
        facts
            .iter()
            .any(|(id, action, outcome, reason)| id == "import-full"
                && action == "automations.import"
                && outcome == "rejected"
                && reason.as_deref() == Some("over_capacity")),
        "rejection audit recorded: {facts:?}"
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn import_overwrite_requires_the_capability() {
    let pool = isolated_pool().await;
    let seed = parse(&json!([{"command": "faq", "response": "A"}]));
    service::import(&pool, GUILD, ACTOR, &seed, false, "import-seed", AT)
        .await
        .expect("seed applies");
    let changed = parse(&json!({
        "commands": [{"command": "FAQ!", "response": "B"}],
        "overwrite": true,
    }));
    let error = service::import(&pool, GUILD, ACTOR, &changed, false, "import-deny", AT)
        .await
        .expect_err("overwrite without capability is refused");
    assert!(
        matches!(error, service::ImportServiceError::OverwriteNotAllowed),
        "capability error, got {error:?}"
    );
    assert_eq!(
        store::get_command(&pool, GUILD, "faq")
            .await
            .expect("read faq")
            .expect("faq stored")
            .template,
        "A",
        "refused overwrite writes nothing"
    );
    assert!(
        !audit_rows(&pool)
            .await
            .iter()
            .any(|(id, _, _, _)| id.starts_with("import-deny")),
        "capability refusal audits nothing"
    );

    // Same content converges (legacy `repeated imports converge`).
    let repeated = service::import(&pool, GUILD, ACTOR, &seed, false, "import-repeat", AT)
        .await
        .expect("repeat applies");
    assert_eq!(repeated.imported, 1);
    assert_eq!(repeated.skipped, 0);
    pool.close().await;
}
