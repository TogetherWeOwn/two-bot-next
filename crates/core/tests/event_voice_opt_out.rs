//! Builder-owned opt-out validation for the existing audited admin settings path.
use serde_json::json;
use two_bot_core::settings::{
    parse_event_voice_opt_outs, validate_write, SettingClass, WriteAction, EVENT_VOICE_OPT_OUT_KEY,
};

#[test]
fn event_voice_opt_outs_are_canonical_bounded_event_ids() {
    let ids = parse_event_voice_opt_outs(&json!(["500", "600", "500"])).unwrap();
    assert_eq!(ids.into_iter().collect::<Vec<_>>(), ["500", "600"]);
    assert!(parse_event_voice_opt_outs(&json!([])).unwrap().is_empty());
    for invalid in [
        json!(null),
        json!("500"),
        json!({"500":true}),
        json!([500]),
        json!(["0"]),
        json!(["0500"]),
        json!([" 500"]),
        json!(["18446744073709551616"]),
        json!(["500", false]),
        json!(vec!["500"; 101]),
    ] {
        assert!(parse_event_voice_opt_outs(&invalid).is_none(), "{invalid}");
    }
}

#[test]
fn event_voice_opt_out_writes_use_existing_audited_settings_and_allow_reset() {
    let write = validate_write(
        "100",
        EVENT_VOICE_OPT_OUT_KEY,
        Some(json!(["500"])),
        "moderator",
    )
    .unwrap();
    assert_eq!(write.class, SettingClass::Hot);
    assert_eq!(write.action, WriteAction::Upsert(json!(["500"])));
    for value in [
        json!(1),
        json!(null),
        json!("500,600"),
        json!({"500": true}),
        json!([500]),
        json!(["0"]),
        json!(["0500"]),
        json!([" 500"]),
        json!(["18446744073709551616"]),
        json!(vec!["500"; 101]),
    ] {
        let refusal = validate_write(
            "100",
            EVENT_VOICE_OPT_OUT_KEY,
            Some(value.clone()),
            "moderator",
        )
        .expect_err("malformed opt-outs must be refused before storage");
        assert_eq!(
            refusal,
            two_bot_core::settings::WriteRefusal::InvalidEventVoiceOptOuts(
                EVENT_VOICE_OPT_OUT_KEY.to_owned()
            ),
            "{value}"
        );
        assert!(refusal.to_string().contains(EVENT_VOICE_OPT_OUT_KEY));
        assert!(parse_event_voice_opt_outs(&value).is_none());
    }
    for value in [json!([]), json!(["500", "500"]), json!(vec!["500"; 100])] {
        let accepted = validate_write(
            "100",
            EVENT_VOICE_OPT_OUT_KEY,
            Some(value.clone()),
            "moderator",
        )
        .unwrap();
        assert_eq!(accepted.action, WriteAction::Upsert(value));
    }
    assert!(validate_write("100", EVENT_VOICE_OPT_OUT_KEY, Some(json!([])), "").is_err());
    assert_eq!(
        validate_write("100", EVENT_VOICE_OPT_OUT_KEY, None, "moderator")
            .unwrap()
            .action,
        WriteAction::Delete
    );
}
