//! Postgres persistence for custom commands and the shared automation audit log.
//!
//! Mirrors legacy `src/automations/store.ts`. Validation and outcome decisions
//! live in `custom_commands`; this module only binds values and maps rows.
//! Capacity reads, writes and audit can share a transaction holding the guild
//! advisory lock. Audit records contain ids/outcomes, never member content.

// Both pools and transaction connections implement Executor. Passing &mut *tx
// keeps capacity reads, writes and audit on the connection holding the lock.
// https://docs.rs/sqlx/0.9.0/sqlx/trait.Executor.html
use sqlx::{Executor, Postgres, Row};

use super::custom_commands::{AuditRecord, StoredCommand};

/// Append an audit fact. The caller supplies the id and ISO-8601 UTC timestamp.
pub async fn audit<'c>(
    executor: impl Executor<'c, Database = Postgres>,
    record: &AuditRecord,
    id: &str,
    at_iso: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO automation_audit_log (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::timestamptz)",
    )
    .bind(id)
    .bind(&record.guild_id)
    .bind(&record.actor_id)
    .bind(&record.action)
    .bind(&record.target_key)
    .bind(&record.outcome)
    .bind(&record.reason)
    .bind(at_iso)
    .execute(executor)
    .await?;
    Ok(())
}

/// Read one audit fact's `(outcome, reason)` by id. Replay paths use this to
/// return the first result verbatim; `Ok(None)` means no such row (a receipt
/// predating the summary row, or a rolled-back apply that never committed).
pub async fn load_audit(
    pool: &PgPool,
    id: &str,
) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    sqlx::query_as("SELECT outcome, reason FROM automation_audit_log WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Permanently reserve one text invocation before any Discord POST. The audit
/// primary key excludes concurrent/replayed attempts even after Discord's nonce
/// window expires. Call with the pool (autocommit), NOT a transaction that could
/// roll back after sending. An error/unknown commit result never permits a send.
///
/// This append-only attempt is intentionally not a lease: crashes may lose a
/// reply, but must never replay an unknown external mutation. The caller appends
/// a separate command.run result when known; an attempt alone means unknown.
pub async fn claim_text_attempt(
    pool: &sqlx::PgPool,
    guild_id: &str,
    actor_id: &str,
    name: &str,
    message_id: u64,
    at_iso: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO automation_audit_log (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
         VALUES ($1, $2, $3, 'command.text_attempt', $4, 'unknown', 'delivery_pending', $5::text::timestamptz)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(format!("custom:text:attempt:{message_id}"))
    .bind(guild_id)
    .bind(actor_id)
    .bind(name)
    .bind(at_iso)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

fn map_command(row: &sqlx::postgres::PgRow) -> StoredCommand {
    StoredCommand {
        guild_id: row.get("guild_id"),
        name: row.get("name"),
        description: row.get("description"),
        template: row.get("template"),
        text_trigger: row.get("text_trigger"),
        enabled: row.get("enabled"),
    }
}

// sqlx 0.9 audits dynamic SQL (SqlSafeStr). Statements stay literals; only
// values ride bind parameters.
macro_rules! select_commands {
    ($where:literal) => {
        concat!(
            "SELECT guild_id, name, description, template, text_trigger, enabled ",
            "FROM automation_commands ",
            $where
        )
    };
}

/// All definitions for one guild, ascending by name.
pub async fn list_commands<'c>(
    executor: impl Executor<'c, Database = Postgres>,
    guild_id: &str,
) -> Result<Vec<StoredCommand>, sqlx::Error> {
    let rows = sqlx::query(select_commands!("WHERE guild_id = $1 ORDER BY name"))
        .bind(guild_id)
        .fetch_all(executor)
        .await?;
    Ok(rows.iter().map(map_command).collect())
}

/// One definition by name.
pub async fn get_command<'c>(
    executor: impl Executor<'c, Database = Postgres>,
    guild_id: &str,
    name: &str,
) -> Result<Option<StoredCommand>, sqlx::Error> {
    let row = sqlx::query(select_commands!("WHERE guild_id = $1 AND name = $2"))
        .bind(guild_id)
        .bind(name)
        .fetch_optional(executor)
        .await?;
    Ok(row.as_ref().map(map_command))
}

/// Enabled definition mapped to the first `!trigger` token, case-insensitively.
pub async fn find_text_trigger<'c>(
    executor: impl Executor<'c, Database = Postgres>,
    guild_id: &str,
    trigger: &str,
) -> Result<Option<StoredCommand>, sqlx::Error> {
    let row = sqlx::query(select_commands!(
        "WHERE guild_id = $1 AND enabled AND lower(text_trigger) = lower($2)"
    ))
    .bind(guild_id)
    .bind(trigger)
    .fetch_optional(executor)
    .await?;
    Ok(row.as_ref().map(map_command))
}

/// Any definition holding a trigger, including a disabled one (import checks).
pub async fn get_command_by_text_trigger<'c>(
    executor: impl Executor<'c, Database = Postgres>,
    guild_id: &str,
    trigger: &str,
) -> Result<Option<StoredCommand>, sqlx::Error> {
    let row = sqlx::query(select_commands!(
        "WHERE guild_id = $1 AND lower(text_trigger) = lower($2)"
    ))
    .bind(guild_id)
    .bind(trigger)
    .fetch_optional(executor)
    .await?;
    Ok(row.as_ref().map(map_command))
}

/// Upsert a validated definition, preserving its original creator/timestamp.
/// The unique trigger index protects against competing definitions.
pub async fn put_command<'c>(
    executor: impl Executor<'c, Database = Postgres>,
    row: &StoredCommand,
    created_by: &str,
    updated_by: &str,
    created_at_iso: &str,
    updated_at_iso: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO automation_commands
           (guild_id, name, description, template, text_trigger, enabled,
            created_by, created_at, updated_by, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::timestamptz, $9, $10::text::timestamptz)
         ON CONFLICT (guild_id, name) DO UPDATE SET
           description  = excluded.description,
           template     = excluded.template,
           text_trigger = excluded.text_trigger,
           enabled      = excluded.enabled,
           updated_by   = excluded.updated_by,
           updated_at   = excluded.updated_at",
    )
    .bind(&row.guild_id)
    .bind(&row.name)
    .bind(&row.description)
    .bind(&row.template)
    .bind(&row.text_trigger)
    .bind(row.enabled)
    .bind(created_by)
    .bind(created_at_iso)
    .bind(updated_by)
    .bind(updated_at_iso)
    .execute(executor)
    .await?;
    Ok(())
}

/// Delete one definition; true when a row went away.
pub async fn delete_command<'c>(
    executor: impl Executor<'c, Database = Postgres>,
    guild_id: &str,
    name: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM automation_commands WHERE guild_id = $1 AND name = $2")
        .bind(guild_id)
        .bind(name)
        .execute(executor)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Serialize a guild's capacity check + write inside the supplied transaction.
/// Unrelated guilds use different advisory keys.
pub async fn lock_command_capacity(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    guild_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("automation_commands:{guild_id}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}
