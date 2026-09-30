//! Validated settings commands and legacy result shapes. No environment reads.

use serde_json::{json, Map, Value};

use crate::internal_actions::{
    check_setting_value_size, require_settings_key, require_snowflake, ActionError, ErrorCode,
};

/// Only the parser can construct a command; values are redacted from Debug.
#[derive(Clone)]
pub struct SettingsCommand {
    key: String,
    write: Option<SettingsWrite>,
}

#[derive(Clone)]
struct SettingsWrite {
    value: Option<Value>,
    actor: String,
    expected_version: Option<i64>,
}

impl std::fmt::Debug for SettingsCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsCommand")
            .field("key", &self.key)
            .field("write", &self.write.is_some())
            .finish_non_exhaustive()
    }
}

impl SettingsCommand {
    /// Validate before reaching any store. Null deletes; omission is malformed.
    /// An optional expected_version is a row version, with zero meaning absent.
    pub fn parse(action: &str, body: &Map<String, Value>) -> Result<Self, ActionError> {
        if !matches!(action, "settings.get" | "settings.set") {
            return Err(ActionError::new(
                ErrorCode::ActionNotAllowed,
                "Not a settings action",
                "settings_action_unknown",
            ));
        }
        let key = require_settings_key(body, action)?.to_owned();
        let write = if action == "settings.set" {
            let actor = require_snowflake(body, "updated_by")?.to_owned();
            let value = body.get("value").ok_or_else(|| {
                ActionError::new(
                    ErrorCode::Malformed,
                    "\"value\" is required; send null to unset the key",
                    "missing_value",
                )
            })?;
            check_setting_value_size(value)?;
            let expected_version = body
                .get("expected_version")
                .map(|v| {
                    v.as_i64().filter(|v| *v >= 0).ok_or_else(|| {
                        ActionError::new(
                            ErrorCode::Malformed,
                            "\"expected_version\" must be a non-negative integer",
                            "bad_expected_version",
                        )
                    })
                })
                .transpose()?;
            Some(SettingsWrite {
                value: (!value.is_null()).then(|| value.clone()),
                actor,
                expected_version,
            })
        } else {
            None
        };
        Ok(Self { key, write })
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// None is a read. The inner None is a delete, not a read.
    #[must_use]
    pub fn write(&self) -> Option<(Option<&Value>, &str, Option<i64>)> {
        self.write
            .as_ref()
            .map(|w| (w.value.as_ref(), w.actor.as_str(), w.expected_version))
    }
}

/// The result goes on the wire; only outcome goes in structured logs. Version
/// is transport metadata for optimistic writes, never an extra legacy field.
#[derive(Clone, PartialEq)]
pub struct SettingsOutcome {
    pub result: Value,
    pub outcome: String,
    pub observed_version: i64,
}

impl std::fmt::Debug for SettingsOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsOutcome")
            .field("outcome", &self.outcome)
            .field("observed_version", &self.observed_version)
            .finish_non_exhaustive()
    }
}

impl SettingsOutcome {
    #[must_use]
    pub fn read(key: &str, stored: Option<(Value, i64)>) -> Self {
        let (value, source, observed_version) = match stored {
            Some((value, version)) => (value, "store", version),
            None => (Value::Null, "unset", 0),
        };
        Self {
            result: json!({"key": key, "value": value, "source": source}),
            outcome: format!("read {key} ({source})"),
            observed_version,
        }
    }

    #[must_use]
    pub fn written(key: &str, deleted: bool, observed_version: i64) -> Self {
        let outcome = if deleted { "unset" } else { "saved" };
        Self {
            result: json!({"key": key, "outcome": outcome}),
            outcome: format!("{outcome} {key}"),
            observed_version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "TWO_RAID_JOIN_THRESHOLD";
    const ADMIN: &str = "111111111111111111";

    fn parse(action: &str, body: Value) -> Result<SettingsCommand, ActionError> {
        SettingsCommand::parse(action, body.as_object().unwrap())
    }

    #[test]
    fn legacy_results_match_bytes_and_log_no_values() {
        let read = SettingsOutcome::read(KEY, Some((json!("8"), 42)));
        assert_eq!(
            read.result.to_string(),
            r#"{"key":"TWO_RAID_JOIN_THRESHOLD","value":"8","source":"store"}"#
        );
        assert_eq!(read.outcome, "read TWO_RAID_JOIN_THRESHOLD (store)");
        assert_eq!(read.observed_version, 42);
        let unset = SettingsOutcome::read(KEY, None);
        assert_eq!(
            unset.result.to_string(),
            r#"{"key":"TWO_RAID_JOIN_THRESHOLD","value":null,"source":"unset"}"#
        );
        for (deleted, expected) in [(false, "saved"), (true, "unset")] {
            let result = SettingsOutcome::written(KEY, deleted, 0);
            assert_eq!(
                result.result.to_string(),
                format!(r#"{{"key":"TWO_RAID_JOIN_THRESHOLD","outcome":"{expected}"}}"#)
            );
        }
        let secret_shaped = "https://example.invalid/hook/private-value";
        let command = parse(
            "settings.set",
            json!({
                "key": KEY, "value": secret_shaped, "updated_by": ADMIN
            }),
        )
        .unwrap();
        assert!(!format!("{command:?}").contains(secret_shaped));
        let read = SettingsOutcome::read(KEY, Some((json!(secret_shaped), 1)));
        assert!(!format!("{read:?}").contains(secret_shaped));
        assert!(!read.outcome.contains(secret_shaped));
    }

    #[test]
    fn refuses_secrets_env_only_unknown_and_malformed_before_execution() {
        for (key, reason) in [
            ("DISCORD_TOKEN", "settings_key_env_only"),
            ("TWO_INTERNAL_KEYS", "settings_key_env_only"),
            ("TWO_INTERNAL_NEW_GATE", "settings_key_env_only"),
            ("TWO_MODERATION", "settings_key_env_only"),
            ("UNKNOWN_KEY", "settings_key_unknown"),
            ("lowercase", "settings_key_malformed"),
        ] {
            for action in ["settings.get", "settings.set"] {
                let error = parse(
                    action,
                    json!({
                        "key": key, "value": "private-value", "updated_by": ADMIN
                    }),
                )
                .unwrap_err();
                assert_eq!(error.log_reason, reason);
                assert!(!format!("{error:?}").contains("private-value"));
            }
        }
    }

    #[test]
    fn null_is_delete_and_actor_value_and_version_are_validated() {
        let command = parse(
            "settings.set",
            json!({
                "key": KEY, "value": null, "updated_by": ADMIN, "expected_version": 0
            }),
        )
        .unwrap();
        assert_eq!(command.write(), Some((None, ADMIN, Some(0))));
        for body in [
            json!({"key": KEY, "updated_by": ADMIN}),
            json!({"key": KEY, "value": 8, "updated_by": "name"}),
            json!({"key": KEY, "value": "x".repeat(8193), "updated_by": ADMIN}),
        ] {
            assert_eq!(
                parse("settings.set", body).unwrap_err().code,
                ErrorCode::Malformed
            );
        }
        for version in [
            json!(-1),
            json!(1.5),
            json!("1"),
            Value::Null,
            json!(u64::MAX),
        ] {
            let error = parse(
                "settings.set",
                json!({
                    "key": KEY, "value": 8, "updated_by": ADMIN, "expected_version": version
                }),
            )
            .unwrap_err();
            assert_eq!(error.log_reason, "bad_expected_version");
        }
        assert!(parse("settings.get", json!({"key": KEY}))
            .unwrap()
            .write()
            .is_none());
        assert_eq!(
            parse("event.read", json!({"key": KEY})).unwrap_err().code,
            ErrorCode::ActionNotAllowed
        );
    }
}
