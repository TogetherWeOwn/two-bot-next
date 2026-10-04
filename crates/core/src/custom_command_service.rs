//! Transaction boundary for the custom-command runtime. No Discord I/O.
//!
//! Validation, capacity and mutation audit share the guild lock. Callers only
//! publish after commit; a Discord failure must not claim the write rolled back.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;

use crate::automation_transfer::{diff_import, ImportOutcome, ParsedImport};
use crate::custom_command_store as store;
use crate::custom_commands::{
    adjudicate_delete, adjudicate_put, builtin_command_names, check_capacity, error_code,
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

/// Import failure. The receiver maps parse/capacity/validation to `Malformed`,
/// overwrite refusal to `ActionNotAllowed`, and storage to `Internal`.
#[derive(Debug, thiserror::Error)]
pub enum ImportServiceError {
    #[error(transparent)]
    Parse(#[from] crate::automation_transfer::ImportParseError),
    #[error("Destructive automation imports are not enabled on this bot.")]
    OverwriteNotAllowed,
    #[error("Write would define more than {0} custom commands, but that is the guild limit.")]
    OverCapacity(usize),
    #[error("import planning failed: {0}")]
    Validation(#[from] CommandError),
    #[error("Custom-command storage failed.")]
    Storage(#[from] sqlx::Error),
}

/// Transactional import of a parsed [`ParsedImport`] (legacy `importMee6`).
/// Validation, capacity, trigger ownership and every write share the guild
/// lock, so the import is atomic: capacity overflow writes nothing (legacy
/// `CommandCapacityError` path). Overwrite mode needs the separately
/// configured `overwrite_allowed` capability on top of the base automations
/// flag, mirroring `TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE` gating
/// `TWO_INTERNAL_ALLOW_AUTOMATIONS` in the receiver.
///
/// Same-content rows are rewritten and count as imported (legacy convergence:
/// a repeated import returns the same counts), so export stays a fixed point.
/// Updates preserve the stored `enabled` flag and creator provenance, exactly
/// like the legacy loop. Per-row audits and the `automations.import` summary
/// commit inside the transaction (the legacy wrote them after commit; the
/// port keeps them atomic with the `put` path). Audit ids derive
/// deterministically from `audit_id`, so a retried internal action with the
/// same key replays the receiver's stored result instead of minting new rows.
#[allow(clippy::too_many_arguments)]
pub async fn import(
    pool: &PgPool,
    guild: &str,
    actor: &str,
    parsed: &ParsedImport,
    overwrite_allowed: bool,
    audit_id: &str,
    at: &str,
) -> Result<ImportOutcome, ImportServiceError> {
    if parsed.overwrite && !overwrite_allowed {
        return Err(ImportServiceError::OverwriteNotAllowed);
    }
    let builtins = builtin_command_names();
    let mut tx = pool.begin().await?;
    store::lock_command_capacity(&mut tx, guild).await?;
    let existing = store::list_commands(&mut *tx, guild).await?;
    let diff = match diff_import(&existing, parsed, &builtins, parsed.overwrite) {
        Ok(diff) => diff,
        Err(err) => {
            // Nothing is written; the rejection fact must survive the
            // rollback, so it commits on its own connection.
            tx.rollback().await?;
            let record = AuditRecord {
                guild_id: guild.to_owned(),
                actor_id: Some(actor.to_owned()),
                action: "automations.import".to_owned(),
                target_key: None,
                outcome: "rejected".to_owned(),
                reason: Some(error_code(&err).to_owned()),
            };
            store::audit(pool, &record, audit_id, at).await?;
            return Err(match err {
                CommandError::OverCapacity(max) => ImportServiceError::OverCapacity(max),
                err => ImportServiceError::Validation(err),
            });
        }
    };
    let planned: HashSet<&str> = diff
        .to_create
        .iter()
        .chain(diff.to_update.iter())
        .chain(diff.unchanged.iter())
        .map(String::as_str)
        .collect();
    let preexisting: HashSet<&str> = existing.iter().map(|row| row.name.as_str()).collect();
    // Enabled flags as the sequential legacy loop would see them: stored rows
    // first, then earlier rows of this same import.
    let mut enabled: HashMap<&str, bool> = existing
        .iter()
        .map(|row| (row.name.as_str(), row.enabled))
        .collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut occurrences: HashMap<&str, usize> = HashMap::new();
    let mut imported = 0usize;
    for entry in &parsed.entries {
        let name: &str = &entry.name;
        if !planned.contains(name) {
            continue;
        }
        let occurrence = occurrences.get(name).copied().unwrap_or(0);
        occurrences.insert(name, occurrence + 1);
        let live = enabled.get(name).copied().unwrap_or(true);
        let row = StoredCommand {
            guild_id: guild.to_owned(),
            name: entry.name.clone(),
            description: entry.description.clone(),
            template: entry.template.clone(),
            text_trigger: entry.text_trigger.clone(),
            enabled: live,
        };
        // `created_by`/`created_at` only matter for fresh rows; the upsert
        // preserves them on conflict.
        store::put_command(&mut *tx, &row, actor, actor, at, at).await?;
        enabled.insert(name, live);
        let created = !seen.contains(name) && !preexisting.contains(name);
        seen.insert(name);
        let audit = AuditRecord::put(guild, actor, &entry.name, created);
        let row_audit_id = format!("{audit_id}#cmd:{}#{occurrence}", entry.name);
        store::audit(&mut *tx, &audit, &row_audit_id, at).await?;
        imported += 1;
    }
    let mut conflicts = parsed.translation_conflicts.clone();
    conflicts.extend(
        diff.rejected
            .iter()
            .filter(|rejection| {
                matches!(
                    rejection.code.as_str(),
                    "would_overwrite" | "trigger_in_use"
                )
            })
            .map(|rejection| rejection.name.clone()),
    );
    let skipped = parsed.invalid_entries + diff.rejected.len();
    let summary = AuditRecord {
        guild_id: guild.to_owned(),
        actor_id: Some(actor.to_owned()),
        action: "automations.import".to_owned(),
        target_key: None,
        outcome: format!(
            "imported:{imported},skipped:{skipped},conflicts:{}",
            conflicts.len()
        ),
        reason: None,
    };
    store::audit(&mut *tx, &summary, &format!("{audit_id}#summary"), at).await?;
    tx.commit().await?;
    Ok(ImportOutcome {
        imported,
        skipped,
        conflicts,
    })
}
