//! Settings command parser acceptance (TOG-12699).
//!
//! Pure, offline: pins the public `two_bot_core::internal_settings` API only —
//! no store, no environment, no network. Spec:
//! `docs/internal-action-settings.md` (`SettingsCommand::parse` validated
//! settings.get/set shapes; values redacted from Debug; CAS `expected_version`;
//! wired to the `require_settings_key` / `is_storable_key` guards). Execution,
//! CAS conflict semantics and storage live in the cutover executor and the
//! ignored `settings_db` suite; see `docs/internal-settings-acceptance.md`.
//!
//! Every `TWO_*` literal below is already classified in
//! `wrangler/src/container-env.ts`; the drift test fails on new names.

use serde_json::{json, Map, Value};
use two_bot_core::internal_actions::{
    is_storable_key, require_settings_key, ActionError, ErrorCode,
};
use two_bot_core::internal_settings::{SettingsCommand, SettingsOutcome};

const KEY: &str = "TWO_RAID_JOIN_THRESHOLD";
const ADMIN: &str = "111111111111111111";
const SECRET_VALUE: &str = "https://example.invalid/hook/private-value";

fn body(payload: Value) -> Map<String, Value> {
    payload.as_object().expect("test body is an object").clone()
}

fn parse(action: &str, payload: Value) -> Result<SettingsCommand, ActionError> {
    SettingsCommand::parse(action, &body(payload))
}

// ---------------------------------------------------------------------------
// (1) Valid shapes parse: get with a storable key, set with value + actor
// ---------------------------------------------------------------------------

#[test]
fn get_accepts_a_storable_key_and_parses_as_a_read() {
    let command = parse("settings.get", json!({"key": KEY})).unwrap();
    assert_eq!(command.key(), KEY);
    assert!(command.write().is_none(), "a read carries no write");
}

#[test]
fn set_accepts_a_value_a_snowflake_actor_and_an_optional_cas_token() {
    let command = parse(
        "settings.set",
        json!({"key": KEY, "value": "8", "updated_by": ADMIN, "expected_version": 42}),
    )
    .unwrap();
    assert_eq!(command.key(), KEY);
    let (value, actor, version) = command.write().expect("set is a write");
    assert_eq!(value.cloned(), Some(json!("8")));
    assert_eq!(actor, ADMIN);
    assert_eq!(version, Some(42));

    // Omitting the token keeps the legacy unconditional-save behavior.
    let command = parse(
        "settings.set",
        json!({"key": KEY, "value": "8", "updated_by": ADMIN}),
    )
    .unwrap();
    assert_eq!(command.write().expect("set is a write").2, None);

    // Negative tokens are opaque CAS tokens, not magnitudes.
    let command = parse(
        "settings.set",
        json!({"key": KEY, "value": 8, "updated_by": ADMIN, "expected_version": -1}),
    )
    .unwrap();
    assert_eq!(command.write().expect("set is a write").2, Some(-1));
}

// ---------------------------------------------------------------------------
// (2) Malformed shapes refuse with the documented error codes
// ---------------------------------------------------------------------------

#[test]
fn unknown_action_refuses_with_action_not_allowed() {
    let error = parse("event.read", json!({"key": KEY})).unwrap_err();
    assert_eq!(error.code, ErrorCode::ActionNotAllowed);
    assert_eq!(error.status(), 403);
    assert!(!error.code.retryable());
    assert_eq!(error.log_reason, "settings_action_unknown");
}

#[test]
fn unknown_malformed_and_env_only_keys_refuse_with_documented_codes() {
    // Valid shape but no catalog entry: almost always a typo.
    let error = parse("settings.get", json!({"key": "UNKNOWN_KEY"})).unwrap_err();
    assert_eq!(error.code, ErrorCode::ActionNotAllowed);
    assert_eq!(error.log_reason, "settings_key_unknown");

    // Bad shape never reaches the catalog.
    let error = parse("settings.get", json!({"key": "lowercase"})).unwrap_err();
    assert_eq!(error.code, ErrorCode::Malformed);
    assert_eq!(error.log_reason, "settings_key_malformed");

    // Declared environment-only keys stay unreachable from both actions,
    // without echoing the submitted value.
    for key in ["DISCORD_TOKEN", "TWO_INTERNAL_KEYS"] {
        for action in ["settings.get", "settings.set"] {
            let error = parse(
                action,
                json!({"key": key, "value": "private-value", "updated_by": ADMIN}),
            )
            .unwrap_err();
            assert_eq!(error.code, ErrorCode::ActionNotAllowed, "{action} {key}");
            assert_eq!(error.log_reason, "settings_key_env_only", "{action} {key}");
            assert!(
                !format!("{error:?}").contains("private-value"),
                "{action} {key}: refusal must not echo the value"
            );
        }
    }
}

#[test]
fn parser_agrees_with_the_settings_key_guards() {
    assert!(is_storable_key(KEY));
    assert!(!is_storable_key("UNKNOWN_KEY"));
    assert!(!is_storable_key("DISCORD_TOKEN"));
    for action in ["settings.get", "settings.set"] {
        assert!(require_settings_key(&body(json!({"key": KEY})), action).is_ok());
        assert_eq!(
            require_settings_key(&body(json!({"key": "UNKNOWN_KEY"})), action)
                .unwrap_err()
                .log_reason,
            "settings_key_unknown",
            "{action}"
        );
    }
}

#[test]
fn set_requires_a_value_and_a_snowflake_audit_actor() {
    let error = parse("settings.set", json!({"key": KEY, "updated_by": ADMIN})).unwrap_err();
    assert_eq!(error.code, ErrorCode::Malformed);
    assert_eq!(error.log_reason, "missing_value");

    let error = parse("settings.set", json!({"key": KEY, "value": 8})).unwrap_err();
    assert_eq!(error.code, ErrorCode::Malformed);
    assert_eq!(error.log_reason, "missing_updated_by");

    for actor in ["name", "123", "111111111111111111 "] {
        let error = parse(
            "settings.set",
            json!({"key": KEY, "value": 8, "updated_by": actor}),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Malformed, "actor {actor:?}");
        assert_eq!(error.log_reason, "bad_updated_by", "actor {actor:?}");
    }
}

#[test]
fn nul_values_and_non_integer_versions_refuse_without_echoing_input() {
    for value in [
        json!("private-value\0"),
        json!({"nested": ["private-value\0"]}),
        json!({"nested": {"private-key\0": 8}}),
    ] {
        let error = parse(
            "settings.set",
            json!({"key": KEY, "value": value, "updated_by": ADMIN}),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Malformed);
        assert_eq!(error.status(), 400);
        assert!(!error.code.retryable());
        assert_eq!(error.log_reason, "settings_value_nul");
        assert!(
            !format!("{error:?}").contains("private-"),
            "NUL refusal must not echo the value"
        );
    }
    for version in [json!(1.5), json!("1"), Value::Null, json!(u64::MAX)] {
        let error = parse(
            "settings.set",
            json!({"key": KEY, "value": 8, "updated_by": ADMIN, "expected_version": version}),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Malformed);
        assert_eq!(error.log_reason, "bad_expected_version");
    }
    // A literal backslash-u escape is not a decoded NUL.
    assert!(parse(
        "settings.set",
        json!({"key": KEY, "value": r"literal \u0000 is not a decoded NUL", "updated_by": ADMIN}),
    )
    .is_ok());
}

// ---------------------------------------------------------------------------
// (3) Null value parses as delete (write present, value None), not a read
// ---------------------------------------------------------------------------

#[test]
fn null_value_parses_as_delete_distinct_from_a_read() {
    let delete = parse(
        "settings.set",
        json!({"key": KEY, "value": null, "updated_by": ADMIN, "expected_version": 0}),
    )
    .unwrap();
    let (value, actor, version) = delete.write().expect("null value is a write, not a read");
    assert_eq!(value, None);
    assert_eq!(actor, ADMIN);
    assert_eq!(version, Some(0));

    let read = parse("settings.get", json!({"key": KEY})).unwrap();
    assert!(read.write().is_none(), "a read carries no write at all");
}

// ---------------------------------------------------------------------------
// (4) Debug output redacts values
// ---------------------------------------------------------------------------

#[test]
fn debug_output_redacts_values_but_keeps_the_key() {
    let command = parse(
        "settings.set",
        json!({"key": KEY, "value": SECRET_VALUE, "updated_by": ADMIN}),
    )
    .unwrap();
    let rendered = format!("{command:?}");
    assert!(
        !rendered.contains(SECRET_VALUE),
        "command Debug leaked the value"
    );
    assert!(
        rendered.contains(KEY),
        "command Debug should still name the key"
    );

    let outcome = SettingsOutcome::read(KEY, Some((json!(SECRET_VALUE), 7)));
    let rendered = format!("{outcome:?}");
    assert!(
        !rendered.contains(SECRET_VALUE),
        "outcome Debug leaked the value"
    );
    assert!(
        !outcome.outcome.contains(SECRET_VALUE),
        "log outcome leaked the value"
    );
    assert!(
        outcome.outcome.contains(KEY),
        "log outcome should still name the key"
    );
}

// ---------------------------------------------------------------------------
// (5) SettingsOutcome carries result/outcome/observed_version for logs
// ---------------------------------------------------------------------------

#[test]
fn outcomes_carry_legacy_results_and_log_safe_summaries() {
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
    assert_eq!(unset.outcome, "read TWO_RAID_JOIN_THRESHOLD (unset)");
    assert_eq!(unset.observed_version, 0);

    let saved = SettingsOutcome::written(KEY, false, 43);
    assert_eq!(
        saved.result.to_string(),
        r#"{"key":"TWO_RAID_JOIN_THRESHOLD","outcome":"saved"}"#
    );
    assert_eq!(saved.outcome, "saved TWO_RAID_JOIN_THRESHOLD");
    assert_eq!(saved.observed_version, 43);

    let deleted = SettingsOutcome::written(KEY, true, 0);
    assert_eq!(
        deleted.result.to_string(),
        r#"{"key":"TWO_RAID_JOIN_THRESHOLD","outcome":"unset"}"#
    );
    assert_eq!(deleted.outcome, "unset TWO_RAID_JOIN_THRESHOLD");
    assert_eq!(deleted.observed_version, 0);
}
