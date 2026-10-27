//! SQL execution for validated settings actions. The receiver owns signing,
//! enablement and durable idempotency; this seam never reads environment values.

use two_bot_core::internal_actions::{is_snowflake, ActionError, ErrorCode};
use two_bot_core::internal_settings::{SettingsCommand, SettingsOutcome};
use two_bot_core::settings::WriteRefusal;

use crate::settings::{SettingsStore, SettingsWriteError};

/// Execute after the receiver has authorized the action and (for a write)
/// claimed its durable idempotency key. Never retry an ambiguous commit here.
pub async fn execute_settings(
    store: &SettingsStore<'_>,
    guild_id: &str,
    command: &SettingsCommand,
) -> Result<SettingsOutcome, ActionError> {
    if !is_snowflake(guild_id) {
        return Err(ActionError::new(
            ErrorCode::Malformed,
            "The configured guild must be a Discord id",
            "settings_guild_malformed",
        ));
    }
    let key = command.key();
    if let Some((value, actor, expected_version)) = command.write() {
        let (_, version) = store
            .set_if_version(guild_id, key, value.cloned(), actor, expected_version)
            .await
            .map_err(action_error)?;
        Ok(SettingsOutcome::written(key, value.is_none(), version))
    } else {
        let stored = store.get(guild_id, key).await.map_err(action_error)?;
        Ok(SettingsOutcome::read(key, stored))
    }
}

fn action_error(error: SettingsWriteError) -> ActionError {
    match error {
        SettingsWriteError::SelfRoleRecoveryPending => ActionError::new(
            ErrorCode::ActionNotAllowed,
            "Wait for self-role recovery to drain before changing TWO_SELF_ROLE_PANELS",
            "settings_self_role_recovery_pending",
        ),
        SettingsWriteError::VersionConflict => ActionError::new(
            ErrorCode::VersionConflict,
            "The setting changed; read it again before saving",
            "settings_version_conflict",
        ),
        SettingsWriteError::Refused(reason) => {
            let log_reason = match reason {
                WriteRefusal::EnvOnly(_) => "settings_key_env_only",
                WriteRefusal::Unknown(_) => "settings_key_unknown",
                WriteRefusal::MissingActor => "missing_updated_by",
                WriteRefusal::NullCharacter => {
                    return ActionError::new(
                        ErrorCode::Malformed,
                        "\"value\" cannot contain U+0000",
                        "settings_value_nul",
                    );
                }
                WriteRefusal::InvalidGateValue(_) => {
                    return ActionError::new(
                        ErrorCode::Malformed,
                        "\"value\" must be a boolean, 0 or 1 for this setting",
                        "settings_value_invalid",
                    );
                }
                WriteRefusal::InvalidCooldownValue(_) => {
                    return ActionError::new(
                        ErrorCode::Malformed,
                        "\"value\" must be a non-negative integer number of seconds for this setting",
                        "settings_value_invalid",
                    );
                }
                WriteRefusal::InvalidPanelsValue(_) => {
                    return ActionError::new(
                        ErrorCode::Malformed,
                        "\"value\" must be a JSON array of self-role panels for this setting",
                        "settings_value_invalid",
                    );
                }
                WriteRefusal::InvalidCorrectionCycles(_) => {
                    return ActionError::new(
                        ErrorCode::Malformed,
                        "\"value\" must be a non-negative integer correction count for this setting",
                        "settings_value_invalid",
                    );
                }
                WriteRefusal::InvalidInactivityDays(_) => {
                    return ActionError::new(
                        ErrorCode::Malformed,
                        "\"value\" must be a non-negative integer number of days for this setting",
                        "settings_value_invalid",
                    );
                }
                WriteRefusal::InvalidOnboardingDestination(_) => {
                    return ActionError::new(
                        ErrorCode::Malformed,
                        reason.to_string(),
                        "settings_value_invalid",
                    );
                }
            };
            ActionError::new(
                ErrorCode::ActionNotAllowed,
                "The setting cannot be reached by this action",
                log_reason,
            )
        }
        // Postgres details can contain parameter/row values. Do not return,
        // format or trace the original error at the internal-action boundary.
        SettingsWriteError::Db(_) => ActionError::new(
            ErrorCode::Internal,
            "The config store could not complete the action",
            "settings_store_failed",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_drain_refusal_names_the_safe_next_action() {
        let error = action_error(SettingsWriteError::SelfRoleRecoveryPending);
        assert_eq!(error.code, ErrorCode::ActionNotAllowed);
        assert_eq!(error.log_reason, "settings_self_role_recovery_pending");
        assert!(error.message.contains("TWO_SELF_ROLE_PANELS"));
        assert!(error.message.contains("drain"));
    }

    #[test]
    fn database_errors_are_sanitized_and_conflicts_are_not_retryable() {
        let error = action_error(SettingsWriteError::Db(sqlx::Error::Protocol(
            "database detail contains private-value".into(),
        )));
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(!format!("{error:?}").contains("private-value"));
        let conflict = action_error(SettingsWriteError::VersionConflict);
        assert_eq!(conflict.status(), 409);
        assert_eq!(conflict.code.as_str(), "version_conflict");
        assert!(!conflict.code.retryable());
        let malformed = action_error(SettingsWriteError::Refused(WriteRefusal::NullCharacter));
        assert_eq!(malformed.code, ErrorCode::Malformed);
        assert_eq!(malformed.status(), 400);
        assert!(!malformed.code.retryable());
        assert_eq!(malformed.log_reason, "settings_value_nul");
        let invalid_gate = action_error(SettingsWriteError::Refused(
            WriteRefusal::InvalidGateValue("TWO_SELF_ROLE_DRY_RUN".to_owned()),
        ));
        assert_eq!(invalid_gate.code, ErrorCode::Malformed);
        assert_eq!(invalid_gate.status(), 400);
        assert!(!invalid_gate.code.retryable());
        assert_eq!(invalid_gate.log_reason, "settings_value_invalid");
        for refusal in [
            WriteRefusal::InvalidCooldownValue("TWO_TICKET_COOLDOWN_SECONDS".to_owned()),
            WriteRefusal::InvalidPanelsValue("TWO_SELF_ROLE_PANELS".to_owned()),
            WriteRefusal::InvalidCorrectionCycles("TWO_COMMUNITY_CORRECTION_CYCLES".to_owned()),
            WriteRefusal::InvalidInactivityDays("TWO_INACTIVITY_DAYS".to_owned()),
        ] {
            let invalid = action_error(SettingsWriteError::Refused(refusal));
            assert_eq!(invalid.code, ErrorCode::Malformed);
            assert_eq!(invalid.status(), 400);
            assert!(!invalid.code.retryable());
            assert_eq!(invalid.log_reason, "settings_value_invalid");
        }
    }
}
