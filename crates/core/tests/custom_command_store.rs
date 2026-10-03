#![cfg(feature = "db")]

//! Opt-in database proof, restricted to agent-testdb or the CI service container.
//! Run: cargo test -p two-bot-core --features db --test custom_command_store -- --ignored
//! No DATABASE_URL or inherited application credentials are consulted.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::Row;
use two_bot_core::custom_command_store as store;
use two_bot_core::custom_commands::{
    adjudicate_delete, adjudicate_put, builtin_command_names, check_capacity, AuditRecord,
    CommandError, StoredCommand,
};

const MIGRATION: &str = include_str!("../../cutover/migrations/0130_custom_commands.sql");
const AT: &str = "2026-09-30T01:00:00Z";
const LATER: &str = "2026-09-30T02:00:00Z";

fn definition(guild_id: &str, name: &str, trigger: Option<&str>) -> StoredCommand {
    StoredCommand {
        guild_id: guild_id.to_owned(),
        name: name.to_owned(),
        description: format!("The {name} command"),
        template: "Hello {user}!".to_owned(),
        text_trigger: trigger.map(str::to_owned),
        enabled: true,
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn store_round_trip_and_transaction_guards() -> Result<(), sqlx::Error> {
    let ci_service = std::env::var("CI").as_deref() == Ok("true")
        && std::env::var("TWO_CUSTOM_COMMAND_TEST_CI").as_deref() == Ok("1");
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
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect_with(options)
        .await?;

    // pg_temp shadows all persistent tables. Everything, including DDL, rolls
    // back, even on an assertion failure when the transaction is dropped.
    let mut tx = pool.begin().await?;
    sqlx::query("CREATE TEMP TABLE custom_commands_test_scope (id INTEGER)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL search_path = pg_temp")
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await?;
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await?;

    store::lock_command_capacity(&mut tx, "test-guild-one").await?;
    let mut contender = pool.begin().await?;
    let same_guild: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind("automation_commands:test-guild-one")
            .fetch_one(&mut *contender)
            .await?;
    assert!(!same_guild, "capacity lock excludes the same guild");
    let other_guild: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind("automation_commands:test-guild-two")
            .fetch_one(&mut *contender)
            .await?;
    assert!(other_guild, "unrelated guild remains independent");
    contender.rollback().await?;

    let mut faq = definition("test-guild-one", "faq", Some("!faq"));
    let current = store::get_command(&mut *tx, &faq.guild_id, &faq.name).await?;
    assert!(current.is_none());
    let builtins = builtin_command_names();
    check_capacity(0, true, builtins.len()).expect("under capacity");
    let decision = adjudicate_put(&faq.guild_id, "admin-one", &faq.name, current.as_ref());
    assert!(decision.created && decision.resync_registry);
    store::put_command(&mut *tx, &faq, "admin-one", "admin-one", AT, AT).await?;
    store::audit(&mut *tx, &decision.audit, "create-faq", AT).await?;
    assert_eq!(
        store::get_command(&mut *tx, &faq.guild_id, "faq").await?,
        Some(faq.clone())
    );
    assert_eq!(
        store::find_text_trigger(&mut *tx, &faq.guild_id, "!FAQ").await?,
        Some(faq.clone())
    );

    faq.template = "Updated {username}".to_owned();
    let decision = adjudicate_put(&faq.guild_id, "admin-two", "faq", Some(&faq));
    assert!(!decision.created && decision.resync_registry);
    store::put_command(&mut *tx, &faq, "admin-two", "admin-two", LATER, LATER).await?;
    store::audit(&mut *tx, &decision.audit, "update-faq", LATER).await?;
    let provenance = sqlx::query(
        "SELECT created_by, updated_by, created_at::text, updated_at::text
         FROM automation_commands WHERE guild_id = $1 AND name = 'faq'",
    )
    .bind(&faq.guild_id)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!(provenance.get::<String, _>("created_by"), "admin-one");
    assert_eq!(provenance.get::<String, _>("updated_by"), "admin-two");
    assert!(provenance
        .get::<String, _>("created_at")
        .starts_with("2026-09-30 01:00:00"));
    assert!(provenance
        .get::<String, _>("updated_at")
        .starts_with("2026-09-30 02:00:00"));

    let other = definition("test-guild-two", "faq", Some("!faq"));
    store::put_command(&mut *tx, &other, "admin-one", "admin-one", AT, AT).await?;
    let mut disabled = definition("test-guild-one", "alpha", Some("!alpha"));
    disabled.enabled = false;
    store::put_command(&mut *tx, &disabled, "admin-one", "admin-one", AT, AT).await?;
    assert!(
        store::find_text_trigger(&mut *tx, &disabled.guild_id, "!ALPHA")
            .await?
            .is_none()
    );
    assert_eq!(
        store::get_command_by_text_trigger(&mut *tx, &disabled.guild_id, "!ALPHA").await?,
        Some(disabled.clone())
    );
    assert_eq!(
        store::list_commands(&mut *tx, &faq.guild_id).await?,
        vec![disabled, faq.clone()]
    );
    assert_eq!(
        store::list_commands(&mut *tx, "test-guild-two").await?,
        vec![other.clone()]
    );

    sqlx::query("SAVEPOINT duplicate_trigger")
        .execute(&mut *tx)
        .await?;
    let duplicate = definition("test-guild-one", "duplicate", Some("!faq"));
    let error = store::put_command(&mut *tx, &duplicate, "admin-one", "admin-one", AT, AT)
        .await
        .expect_err("duplicate trigger must fail");
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23505")
    );
    sqlx::query("ROLLBACK TO SAVEPOINT duplicate_trigger")
        .execute(&mut *tx)
        .await?;

    let rejected = AuditRecord::put_rejected(
        &faq.guild_id,
        "admin-one",
        "ban",
        false,
        &CommandError::ReservedName("ban".to_owned()),
    );
    store::audit(&mut *tx, &rejected, "reject-builtin", AT).await?;
    let failed = AuditRecord::run(
        &faq.guild_id,
        "member-one",
        "faq",
        false,
        Some("delivery_failed"),
    );
    store::audit(&mut *tx, &failed, "failed-run", AT).await?;
    let ok = AuditRecord::run(&faq.guild_id, "member-one", "faq", true, None);
    store::audit(&mut *tx, &ok, "successful-run", AT).await?;
    let system = AuditRecord {
        actor_id: None,
        ..ok
    };
    store::audit(&mut *tx, &system, "system-run", AT).await?;

    let gone = store::delete_command(&mut *tx, &faq.guild_id, "faq").await?;
    let decision = adjudicate_delete(&faq.guild_id, "admin-one", "faq", gone);
    assert!(decision.deleted && decision.resync_registry);
    store::audit(&mut *tx, &decision.audit, "delete-faq", AT).await?;
    let gone = store::delete_command(&mut *tx, &faq.guild_id, "faq").await?;
    let decision = adjudicate_delete(&faq.guild_id, "admin-one", "faq", gone);
    assert!(!decision.deleted && !decision.resync_registry);
    store::audit(&mut *tx, &decision.audit, "delete-absent", AT).await?;
    assert_eq!(
        store::get_command(&mut *tx, "test-guild-two", "faq").await?,
        Some(other)
    );

    let facts: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT id, action, outcome, reason FROM automation_audit_log ORDER BY id")
            .fetch_all(&mut *tx)
            .await?;
    assert_eq!(facts.len(), 8);
    assert!(facts.contains(&(
        "create-faq".to_owned(),
        "command.create".to_owned(),
        "ok".to_owned(),
        None
    )));
    assert!(facts.contains(&(
        "reject-builtin".to_owned(),
        "command.create".to_owned(),
        "rejected".to_owned(),
        Some("reserved_name".to_owned())
    )));
    assert!(facts.contains(&(
        "failed-run".to_owned(),
        "command.run".to_owned(),
        "failed".to_owned(),
        Some("delivery_failed".to_owned())
    )));
    assert!(facts.contains(&(
        "delete-absent".to_owned(),
        "command.delete".to_owned(),
        "absent".to_owned(),
        None
    )));

    // A rolled-back outcome leaves neither its command nor its audit fact.
    sqlx::query("SAVEPOINT atomic_outcome")
        .execute(&mut *tx)
        .await?;
    let rolled_back = definition("test-guild-one", "rollback", None);
    store::put_command(&mut *tx, &rolled_back, "admin-one", "admin-one", AT, AT).await?;
    store::audit(&mut *tx, &rejected, "rollback-audit", AT).await?;
    sqlx::query("ROLLBACK TO SAVEPOINT atomic_outcome")
        .execute(&mut *tx)
        .await?;
    assert!(store::get_command(&mut *tx, "test-guild-one", "rollback")
        .await?
        .is_none());
    let absent: i64 =
        sqlx::query_scalar("SELECT count(*) FROM automation_audit_log WHERE id = 'rollback-audit'")
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(absent, 0);
    tx.rollback().await?;
    pool.close().await;
    Ok(())
}
