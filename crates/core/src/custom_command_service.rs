//! Transaction boundary for the custom-command runtime. No Discord I/O.
//!
//! Validation, capacity and mutation audit share the guild lock. Callers only
//! publish after commit; a Discord failure must not claim the write rolled back.

use sqlx::PgPool;

use crate::custom_command_store as store;
use crate::custom_commands::{
    adjudicate_delete, adjudicate_put, builtin_command_names, check_capacity,
    require_automations_enabled, validate_put_input, AuditRecord, CommandError, DeleteDecision,
    PutCommandInput, PutDecision, StoredCommand,
};

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Validation(#[from] CommandError),
    #[error("That text trigger is already assigned to another command.")]
    TriggerInUse,
    #[error("Custom-command storage failed.")]
    Storage(#[from] sqlx::Error),
}

/// The caller uses the interaction ID as an audit ID and supplies one timestamp.
/// Holding the lock across reads closes capacity and trigger ownership races.
#[allow(clippy::too_many_arguments)]
pub async fn put(
    pool: &PgPool,
    enabled: bool,
    guild: &str,
    actor: &str,
    input: &PutCommandInput,
    audit_id: &str,
    at: &str,
) -> Result<PutDecision, ServiceError> {
    require_automations_enabled(enabled)?;
    let builtins = builtin_command_names();
    let mut tx = pool.begin().await?;
    store::lock_command_capacity(&mut tx, guild).await?;
    let existing = store::get_command(&mut *tx, guild, &input.name).await?;
    let rows = store::list_commands(&mut *tx, guild).await?;
    let validation = validate_put_input(input, &builtins)
        .and_then(|()| check_capacity(rows.len(), existing.is_none(), builtins.len()));
    if let Err(err) = validation {
        let record = AuditRecord::put_rejected(guild, actor, &input.name, existing.is_some(), &err);
        store::audit(&mut *tx, &record, audit_id, at).await?;
        tx.commit().await?;
        return Err(err.into());
    }
    if input.text_trigger.as_ref().is_some_and(|trigger| {
        rows.iter().any(|row| {
            row.name != input.name
                && row
                    .text_trigger
                    .as_ref()
                    .is_some_and(|held| held.eq_ignore_ascii_case(trigger))
        })
    }) {
        let mut record = AuditRecord::put(guild, actor, &input.name, existing.is_none());
        record.outcome = "rejected".to_owned();
        record.reason = Some("trigger_in_use".to_owned());
        store::audit(&mut *tx, &record, audit_id, at).await?;
        tx.commit().await?;
        return Err(ServiceError::TriggerInUse);
    }
    let decision = adjudicate_put(guild, actor, &input.name, existing.as_ref());
    let row = StoredCommand {
        guild_id: guild.to_owned(),
        name: input.name.clone(),
        description: input.description.clone(),
        template: input.template.clone(),
        text_trigger: input.text_trigger.clone(),
        enabled: true,
    };
    store::put_command(&mut *tx, &row, actor, actor, at, at).await?;
    store::audit(&mut *tx, &decision.audit, audit_id, at).await?;
    tx.commit().await?;
    Ok(decision)
}

pub async fn delete(
    pool: &PgPool,
    enabled: bool,
    guild: &str,
    actor: &str,
    name: &str,
    audit_id: &str,
    at: &str,
) -> Result<DeleteDecision, ServiceError> {
    require_automations_enabled(enabled)?;
    let mut tx = pool.begin().await?;
    store::lock_command_capacity(&mut tx, guild).await?;
    let deleted = store::delete_command(&mut *tx, guild, name).await?;
    let decision = adjudicate_delete(guild, actor, name, deleted);
    store::audit(&mut *tx, &decision.audit, audit_id, at).await?;
    tx.commit().await?;
    Ok(decision)
}
