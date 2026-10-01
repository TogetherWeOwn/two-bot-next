//! Store-owning website channel moderation. The HTTP receiver still owns HMAC,
//! replay/nonces and actor resolution; this executor never trusts body permissions.
//! Slash wiring must use the same store's channel fence, not execute_outcome's
//! unrecorded unlock fallback. Uncertain effects retain both durable fences.

use crate::{ActionExecutor, DiscordError};
use serde_json::{json, Map, Value};
use two_bot_core::channel_moderation::{self, ChannelOutcome, UnlockPlan};
use two_bot_core::channel_moderation_store::{
    ChannelAuditRow, ChannelClaim, ChannelClaimTicket, ChannelModerationStore,
};
use two_bot_core::internal_actions::{
    body_hash, require_reason, require_snowflake, validate_idempotency_key,
    validate_moderation_numbers, ActionError, ErrorCode,
};
use two_bot_core::mac::moderation_audit_reason;
use two_bot_core::{
    assert_moderation_allowed, ModerationAction, ModerationActor, ModerationPolicy,
    ModerationRequest,
};

/// Parsed website fields; construct only after request authorization. The actor
/// id is an attribution request, not proof of guild membership or permissions.
#[derive(Debug, Clone)]
pub struct InternalChannelRequest {
    action: ModerationAction,
    actor_id: String,
    channel_id: String,
    reason: String,
    duration_seconds: Option<i64>,
    count: Option<i64>,
    seconds: Option<i64>,
}

impl InternalChannelRequest {
    pub fn from_body(action: &str, body: &Map<String, Value>) -> Result<Self, ActionError> {
        let action = match action {
            "moderation.purge" => ModerationAction::Purge,
            "moderation.slowmode" => ModerationAction::Slowmode,
            "moderation.lockdown" => ModerationAction::Lockdown,
            "moderation.unlock" => ModerationAction::Unlock,
            _ => {
                return Err(error(
                    ErrorCode::ActionNotAllowed,
                    "not a channel moderation action",
                ))
            }
        };
        let actor_id = require_snowflake(body, "actor_id")?.to_owned();
        let channel_id = require_snowflake(body, "channel_id")?.to_owned();
        // Tighten the legacy shape check to canonical, nonzero u64 identities:
        // ledger keys and REST identities must never refer to different channels.
        for id in [&actor_id, &channel_id] {
            if !canonical_id(id) {
                return Err(error(ErrorCode::Malformed, "invalid Discord id"));
            }
        }
        let reason = require_reason(body.get("reason").unwrap_or(&Value::Null))?;
        let integer = |field: &str| -> Result<Option<i64>, ActionError> {
            match body.get(field) {
                None => Ok(None),
                Some(value) => value.as_i64().map(Some).ok_or_else(|| {
                    error(ErrorCode::Malformed, format!("{field} must be an integer"))
                }),
            }
        };
        validate_moderation_numbers(
            action,
            body.get("duration_seconds"),
            body.get("count"),
            body.get("seconds"),
        )?;
        Ok(Self {
            action,
            actor_id,
            channel_id,
            reason,
            duration_seconds: integer("duration_seconds")?,
            count: integer("count")?,
            seconds: integer("seconds")?,
        })
    }

    /// Legacy service hash, shared with the slash ledger. Actor is deliberately
    /// absent: replay keeps the first successful audit attribution unchanged.
    fn request_hash(&self, guild_id: &str) -> String {
        body_hash(json!({
            "action": self.action.action_name(), "guildId": guild_id,
            "targetId": null, "channelId": self.channel_id, "reason": self.reason,
            "durationSeconds": self.duration_seconds, "count": self.count, "seconds": self.seconds,
        }).to_string().as_bytes())
    }
}

/// Runtime-only configuration. `enabled` must reflect BOTH TWO_MODERATION and
/// TWO_INTERNAL_ALLOW_MODERATION. No Debug: audit secrets must not enter logs.
#[derive(Clone)]
pub struct InternalChannelConfig {
    pub guild_id: String,
    pub enabled: bool,
    pub policy: ModerationPolicy,
    pub audit_secret: Option<String>,
}

#[derive(Clone)]
pub struct InternalChannelExecutor {
    store: ChannelModerationStore,
    discord: ActionExecutor,
    config: InternalChannelConfig,
}

/// Legacy runModerationAction response, separate from the HTTP envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalChannelResult {
    pub outcome: String,
    pub affected: Option<u64>,
    pub replayed: bool,
}

impl InternalChannelResult {
    #[must_use]
    pub fn response(&self) -> Value {
        let mut result = json!({"outcome": self.outcome});
        if let Some(affected) = self.affected {
            result["affected"] = json!(affected);
        }
        let mut response = json!({"result": result, "outcome": self.outcome});
        if self.replayed {
            response["innerReplayed"] = json!(true);
        }
        response
    }
}

impl InternalChannelExecutor {
    pub fn new(
        store: ChannelModerationStore,
        discord: ActionExecutor,
        config: InternalChannelConfig,
    ) -> Result<Self, ActionError> {
        if !canonical_id(&config.guild_id) {
            return Err(error(ErrorCode::Malformed, "invalid configured guild id"));
        }
        Ok(Self {
            store,
            discord,
            config,
        })
    }

    /// `actor` must be resolved in the configured guild by the runtime (legacy
    /// resolver.actor), NEVER built from caller-supplied roles/permissions. IDs,
    /// policy and enablement are checked before claiming or making any wire call.
    /// request_id/time are server-generated, not website body values.
    pub async fn execute(
        &self,
        request: &InternalChannelRequest,
        actor: &ModerationActor,
        request_id: &str,
        idempotency_key: &str,
        now: &str,
    ) -> Result<InternalChannelResult, ActionError> {
        validate_idempotency_key(Some(idempotency_key), request.action.action_name())?;
        if !self.config.enabled || actor.user_id != request.actor_id {
            return Err(error(
                ErrorCode::ActionNotAllowed,
                "moderation disabled or actor not resolved",
            ));
        }
        let policy_request = ModerationRequest {
            action: request.action,
            actor: actor.clone(),
            target: None,
            bot_highest_role_position: None,
            reason: request.reason.clone(),
            duration_seconds: None,
            count: request.count.and_then(|n| n.try_into().ok()),
            seconds: request.seconds.and_then(|n| n.try_into().ok()),
        };
        assert_moderation_allowed(&policy_request, &self.config.policy)
            .map_err(|e| error(ErrorCode::ActionNotAllowed, e.to_string()))?;
        let claim = self
            .store
            .claim(
                &self.config.guild_id,
                idempotency_key,
                request.action.action_name(),
                &request.request_hash(&self.config.guild_id),
                now,
            )
            .await
            .map_err(database_error)?;
        let ticket = match claim {
            ChannelClaim::Replayed {
                outcome,
                result_json,
            } => {
                let value: Value = serde_json::from_str(&result_json).map_err(|_| {
                    error(ErrorCode::Internal, "unreadable stored moderation result")
                })?;
                return Ok(InternalChannelResult {
                    outcome,
                    affected: value.get("affected").and_then(Value::as_u64),
                    replayed: true,
                });
            }
            ChannelClaim::InFlight => {
                return Err(error(
                    ErrorCode::InProgress,
                    "moderation attempt is still in progress",
                ))
            }
            ChannelClaim::Mismatch => {
                return Err(error(
                    ErrorCode::Malformed,
                    "idempotency key was used for different moderation content",
                ))
            }
            ChannelClaim::Claimed { ticket } => ticket,
        };
        let reserved = match self
            .store
            .reserve_channel(&ticket, &request.channel_id)
            .await
        {
            Ok(reserved) => reserved,
            Err(failure) => {
                // A definitive rejection acquired no channel fence and sent no
                // Discord request. Release only this generation's request claim;
                // unknown completion must retain it for reconciliation.
                if definitive_database_rejection(&failure)
                    && !self.store.release(&ticket).await.map_err(database_error)?
                {
                    return Err(stale_fence());
                }
                return Err(database_error(failure));
            }
        };
        if !reserved {
            if !self.store.release(&ticket).await.map_err(database_error)? {
                return Err(stale_fence());
            }
            return Err(error(
                ErrorCode::InProgress,
                "another moderation attempt holds this channel",
            ));
        }
        let mut audit = ChannelAuditRow {
            request_id: request_id.to_owned(),
            guild_id: self.config.guild_id.clone(),
            actor_id: actor.user_id.clone(),
            action: request.action.action_name().to_owned(),
            channel_id: Some(request.channel_id.clone()),
            reason: request.reason.clone(),
            outcome: "refused".to_owned(),
            idempotency_key: idempotency_key.to_owned(),
            metadata_json:
                json!({"duration_seconds": request.duration_seconds, "count": request.count,
                "seconds": request.seconds, "affected": null})
                .to_string(),
            created_at: now.to_owned(),
        };
        // Read-only failures are always proven pre-mutation, even timeouts.
        let current = match self
            .discord
            .get_guild_channel_overwrite(&request.channel_id, &self.config.guild_id)
            .await
        {
            Ok(current) => current,
            Err(failure) => {
                self.abort(&ticket, &audit, None).await?;
                return Err(discord_error(failure));
            }
        };
        let reason = bounded_audit_reason(
            self.config.audit_secret.as_deref(),
            &self.config.guild_id,
            idempotency_key,
            request.action.action_name(),
            &actor.user_id,
            &request.reason,
        );
        let mut clear_on_success = None;
        let mut clear_on_rejection = None;
        let result = match request.action {
            ModerationAction::Purge => {
                let ids = match self
                    .discord
                    .list_purge_messages(
                        &request.channel_id,
                        request.count.expect("validated count") as u64,
                    )
                    .await
                {
                    Ok(ids) => ids,
                    Err(failure) => {
                        self.abort(&ticket, &audit, None).await?;
                        return Err(discord_error(failure));
                    }
                };
                self.discord
                    .purge_messages(&request.channel_id, &ids, &reason)
                    .await
                    .map(|affected| ChannelOutcome::Purged { affected })
            }
            ModerationAction::Slowmode => self
                .discord
                .set_slowmode(
                    &request.channel_id,
                    request.seconds.expect("validated seconds") as u64,
                    &reason,
                )
                .await
                .map(|()| ChannelOutcome::SlowmodeUpdated),
            ModerationAction::Lockdown => {
                let current = current.map(|ow| channel_moderation::EveryoneOverwrite {
                    allow: ow.allow,
                    deny: ow.deny,
                });
                let plan = match channel_moderation::plan_lockdown(current.as_ref()) {
                    Ok(plan) => plan,
                    Err(_) => {
                        self.abort(&ticket, &audit, None).await?;
                        return Err(error(ErrorCode::Malformed, "invalid channel masks"));
                    }
                };
                let existing = match self.store.get_lockdown(&request.channel_id).await {
                    Ok(record) => record,
                    Err(failure) => {
                        // Recovery lookup is read-only: preserve any seed, but
                        // atomically release both reservations before returning.
                        self.abort(&ticket, &audit, None).await?;
                        return Err(database_error(failure));
                    }
                };
                if existing
                    .as_ref()
                    .is_some_and(|record| record.guild_id != self.config.guild_id)
                {
                    self.abort(&ticket, &audit, None).await?;
                    return Err(error(
                        ErrorCode::ActionNotAllowed,
                        "recovery state belongs to another guild",
                    ));
                }
                let record = match self
                    .store
                    .record_lockdown(
                        &request.channel_id,
                        &self.config.guild_id,
                        &plan.seed,
                        &request.reason,
                        now,
                    )
                    .await
                {
                    Ok(record) => record,
                    Err(failure) => {
                        // Only definitive SQL rejections prove no seed commit;
                        // transport/unknown-completion errors remain fenced.
                        // No Discord PUT was sent. Preserve any original seed.
                        if definitive_database_rejection(&failure) {
                            self.abort(&ticket, &audit, None).await?;
                        }
                        return Err(database_error(failure));
                    }
                };
                if existing.is_none() {
                    clear_on_rejection = Some(record.recovery_generation);
                }
                self.discord
                    .put_everyone_overwrite(
                        &request.channel_id,
                        &self.config.guild_id,
                        &plan.write.allow,
                        &plan.write.deny,
                        &reason,
                    )
                    .await
                    .map(|()| ChannelOutcome::LockedDown)
            }
            ModerationAction::Unlock => {
                let record = match self.store.get_lockdown(&request.channel_id).await {
                    Ok(record) => record,
                    Err(failure) => {
                        // Recovery lookup is read-only: preserve any seed, but
                        // atomically release both reservations before returning.
                        self.abort(&ticket, &audit, None).await?;
                        return Err(database_error(failure));
                    }
                };
                let plan = match channel_moderation::plan_unlock(record.as_ref()) {
                    Ok(plan) => plan,
                    Err(_) => {
                        self.abort(&ticket, &audit, None).await?;
                        return Err(error(
                            ErrorCode::ActionNotAllowed,
                            "channel is not locked down or recovery masks are invalid",
                        ));
                    }
                };
                let record = record.expect("plan requires record");
                if record.guild_id != self.config.guild_id {
                    self.abort(&ticket, &audit, None).await?;
                    return Err(error(
                        ErrorCode::ActionNotAllowed,
                        "recovery state belongs to another guild",
                    ));
                }
                clear_on_success = Some(record.recovery_generation);
                match plan {
                    UnlockPlan::Restore { allow, deny } => {
                        self.discord
                            .put_everyone_overwrite(
                                &request.channel_id,
                                &self.config.guild_id,
                                &allow,
                                &deny,
                                &reason,
                            )
                            .await
                    }
                    UnlockPlan::DeleteOverwrite => {
                        self.discord
                            .delete_everyone_overwrite(
                                &request.channel_id,
                                &self.config.guild_id,
                                &reason,
                            )
                            .await
                    }
                }
                .map(|()| ChannelOutcome::Unlocked)
            }
            _ => unreachable!("parsed channel verb"),
        };
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(failure) => {
                if failure.is_safe_pre_mutation() {
                    self.abort(&ticket, &audit, clear_on_rejection.as_deref())
                        .await?;
                } else {
                    // Never release on uncertain writes. A later distinct-key
                    // unlock/lockdown must not race a delayed Discord request.
                    self.store
                        .record_audit(&audit)
                        .await
                        .map_err(database_error)?;
                }
                return Err(discord_error(failure));
            }
        };
        let affected = match outcome {
            ChannelOutcome::Purged { affected } => Some(affected),
            _ => None,
        };
        audit.outcome = outcome.name().to_owned();
        audit.metadata_json =
            json!({"duration_seconds": request.duration_seconds, "count": request.count,
            "seconds": request.seconds, "affected": affected})
            .to_string();
        let stored = json!({"outcome": audit.outcome, "affected": affected}).to_string();
        if !self
            .store
            .finish_channel(&ticket, &audit, &stored, clear_on_success.as_deref())
            .await
            .map_err(database_error)?
        {
            return Err(stale_fence());
        }
        Ok(InternalChannelResult {
            outcome: audit.outcome,
            affected,
            replayed: false,
        })
    }

    async fn abort(
        &self,
        ticket: &ChannelClaimTicket,
        audit: &ChannelAuditRow,
        clear_generation: Option<&str>,
    ) -> Result<(), ActionError> {
        if !self
            .store
            .abort_channel(ticket, audit, clear_generation)
            .await
            .map_err(database_error)?
        {
            return Err(stale_fence());
        }
        Ok(())
    }
}

fn canonical_id(value: &str) -> bool {
    two_bot_core::internal_actions::is_snowflake(value)
        && value
            .parse::<u64>()
            .is_ok_and(|n| n != 0 && n.to_string() == value)
}

/// Preserve the complete MAC marker, truncating only the unsigned human suffix
/// at scalar boundaries so the final header stays within 512 UTF-16 units.
fn bounded_audit_reason(
    secret: Option<&str>,
    guild: &str,
    key: &str,
    action: &str,
    actor: &str,
    reason: &str,
) -> String {
    let prefix = moderation_audit_reason(secret, guild, key, action, actor, "");
    if prefix.is_empty() {
        return reason.to_owned();
    }
    let mut budget = 512usize.saturating_sub(prefix.encode_utf16().count());
    let suffix: String = reason
        .chars()
        .take_while(|c| {
            if c.len_utf16() > budget {
                false
            } else {
                budget -= c.len_utf16();
                true
            }
        })
        .collect();
    format!("{prefix}{suffix}")
}

fn error(code: ErrorCode, message: impl Into<String>) -> ActionError {
    ActionError::new(code, message, "internal_channel_moderation")
}
fn definitive_database_rejection(failure: &sqlx::Error) -> bool {
    match failure {
        sqlx::Error::Database(db) => db
            .code()
            .is_some_and(|code| definitive_sql_rejection(&code)),
        _ => false,
    }
}

// Data/integrity/syntax-access rejection, serialization failure, deadlock and
// cancellation abort the statement. Do not include class 08, 40003 (statement
// completion unknown), shutdowns, or unrecognized/missing SQLSTATEs.
fn definitive_sql_rejection(code: &str) -> bool {
    code.len() == 5
        && (code.starts_with("22")
            || code.starts_with("23")
            || code.starts_with("42")
            || matches!(code, "40001" | "40P01" | "57014"))
}

fn database_error(_: sqlx::Error) -> ActionError {
    error(
        ErrorCode::Internal,
        "moderation persistence failed; do not repeat an uncertain effect",
    )
}
fn stale_fence() -> ActionError {
    error(
        ErrorCode::Internal,
        "moderation execution or recovery fence is stale",
    )
}
fn discord_error(failure: DiscordError) -> ActionError {
    let code = match failure {
        DiscordError::Rejected(_) => ErrorCode::DiscordRejected,
        DiscordError::Timeout => ErrorCode::UpstreamTimeout,
        DiscordError::RateLimited => ErrorCode::RateLimited,
        DiscordError::Unavailable(_) => ErrorCode::DiscordUnavailable,
    };
    error(code, failure.to_string())
}

#[cfg(test)]
mod tests {
    use super::definitive_sql_rejection;

    #[test]
    fn recovery_sql_rejection_excludes_unknown_completion() {
        for code in [
            "22003", "23502", "23505", "23514", "42501", "42703", "40001", "40P01", "57014",
        ] {
            assert!(definitive_sql_rejection(code), "{code}");
        }
        for code in [
            "", "23", "08006", "08007", "40003", "57P01", "XX000", "ZZZZZ",
        ] {
            assert!(!definitive_sql_rejection(code), "{code}");
        }
    }
}
