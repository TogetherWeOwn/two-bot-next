//! Protected acceptance: refuse unusable onboarding destinations before saving.
//! Builder must incorporate this file unchanged; no implementation is supplied.
//! Break caught: missing write validation admits values readers reject/ignore.

#[path = "support/onboarding_destination_cases.rs"]
mod cases;

use serde_json::json;
use two_bot_core::settings::{
    classify_key, is_live_key, to_env_string, validate_write, SettingClass, WriteAction,
    WriteRefusal,
};

const GUILD: &str = "111111111111111111";
const ACTOR: &str = "222222222222222222";

fn accepts(key: &str) {
    for (value, rendered, _) in cases::valid(key) {
        assert_eq!(to_env_string(&value).as_deref(), Some(rendered), "{key}");
        let write = validate_write(GUILD, key, Some(value.clone()), ACTOR)
            .expect("reader-compatible value must remain writable");
        assert_eq!(write.guild_id, GUILD);
        assert_eq!(write.key, key);
        assert_eq!(write.actor, ACTOR);
        assert_eq!(write.class, SettingClass::Hot);
        assert_eq!(write.action, WriteAction::Upsert(value));
    }
}

fn refuses(key: &str) {
    for value in cases::invalid(key) {
        assert!(
            validate_write(GUILD, key, Some(value), ACTOR).is_err(),
            "{key}: invalid or ignored destination reached the write boundary"
        );
    }
}

macro_rules! destination {
    ($good:ident, $bad:ident, $key:expr) => {
        #[test]
        fn $good() {
            accepts($key);
        }
        #[test]
        fn $bad() {
            refuses($key);
        }
    };
}
destination!(
    anchor_valid_control,
    anchor_invalid_refused,
    cases::SINGLES[0]
);
destination!(
    looking_valid_control,
    looking_invalid_refused,
    cases::SINGLES[1]
);
destination!(
    lobby_valid_control,
    lobby_invalid_refused,
    cases::SINGLES[2]
);
destination!(
    landing_valid_control,
    landing_invalid_refused,
    cases::LISTS[0]
);
destination!(
    goodbye_valid_control,
    goodbye_invalid_refused,
    cases::LISTS[1]
);

#[test]
fn refusal_is_typed_key_only_and_explains_repair_or_delete_to_fallback() {
    for key in cases::ALL {
        let first = validate_write(
            GUILD,
            key,
            Some(json!("private-destination-sentinel")),
            ACTOR,
        )
        .expect_err("bad destination must be refused");
        let second = validate_write(GUILD, key, Some(json!("18446744073709551616")), ACTOR)
            .expect_err("overflow must be refused");
        // A distinct typed value refusal, not a misreported existing guard.
        assert!(!matches!(
            first,
            WriteRefusal::EnvOnly(_)
                | WriteRefusal::Unknown(_)
                | WriteRefusal::MissingActor
                | WriteRefusal::NullCharacter
        ));
        for unrelated in [
            WriteRefusal::InvalidGateValue(key.into()),
            WriteRefusal::InvalidCooldownValue(key.into()),
            WriteRefusal::InvalidPanelsValue(key.into()),
            WriteRefusal::InvalidCorrectionCycles(key.into()),
            WriteRefusal::InvalidInactivityDays(key.into()),
        ] {
            assert_ne!(
                std::mem::discriminant(&first),
                std::mem::discriminant(&unrelated)
            );
        }
        assert_eq!(
            first, second,
            "refusal must depend on the allowlisted key only"
        );
        assert_eq!(format!("{first:?}"), format!("{second:?}"));
        let message = first.to_string();
        assert!(message.contains(key));
        assert!(message.contains("delete"), "must explain the undo action");
        assert!(message.contains("fallback") || message.contains("fall back"));
        for output in [message, format!("{first:?}"), format!("{first:#?}")] {
            for private in [
                "private-destination-sentinel",
                "18446744073709551616",
                GUILD,
                ACTOR,
            ] {
                assert!(!output.contains(private), "refusal leaked supplied data");
            }
        }
    }
}

#[test]
fn deletion_classification_attribution_and_existing_gate_controls() {
    for key in cases::ALL {
        assert_eq!(classify_key(key), Some(SettingClass::Hot));
        // This slice must not promote the currently unwired session/anchor keys.
        assert_eq!(is_live_key(key), cases::LISTS.contains(&key));
        let delete = validate_write(GUILD, key, None, ACTOR).unwrap();
        assert_eq!(delete.action, WriteAction::Delete);
        assert_eq!(delete.actor, ACTOR);
        assert_eq!(
            validate_write(GUILD, key, None, ""),
            Err(WriteRefusal::MissingActor)
        );
        assert_eq!(
            validate_write(GUILD, key, Some(json!("bad\0id")), ACTOR),
            Err(WriteRefusal::NullCharacter)
        );
    }
    for key in [
        "TWO_ONBOARDING_MODE",
        "DISCORD_GUILD_ID",
        "TWO_INTERNAL_ALLOW_SETTINGS",
    ] {
        assert!(matches!(
            validate_write(GUILD, key, Some(json!(42)), ACTOR),
            Err(WriteRefusal::EnvOnly(_))
        ));
    }
    assert!(matches!(
        validate_write(GUILD, "UNKNOWN_DESTINATION", None, ACTOR),
        Err(WriteRefusal::Unknown(_))
    ));
    assert!(matches!(
        validate_write(GUILD, "TWO_ONBOARDING_DRY_RUN", Some(json!(2)), ACTOR),
        Err(WriteRefusal::InvalidGateValue(_))
    ));
    assert!(validate_write(GUILD, "TWO_ONBOARDING_DRY_RUN", Some(json!(true)), ACTOR).is_ok());
}
