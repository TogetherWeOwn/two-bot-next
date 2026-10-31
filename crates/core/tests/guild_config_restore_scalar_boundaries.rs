//! Builder regressions for blank strings and negative Discord-bound restore scalars.
use serde_json::{json, Map, Value};
use two_bot_core::backup::{
    guild_config::{seal_snapshot, verify_snapshot_integrity},
    guild_config_restore::plan_restore,
};

fn snapshot() -> Map<String, Value> {
    serde_json::from_value(json!({
        "version": 1,
        "guildId": "123",
        "guild": {
            "name": "Guild", "preferred_locale": "en-US", "description": null,
            "verification_level": 0, "default_message_notifications": 0,
            "explicit_content_filter": 0, "afk_timeout": 60, "system_channel_flags": 0,
            "system_channel_id": null, "rules_channel_id": null,
            "public_updates_channel_id": null, "afk_channel_id": null
        },
        "roles": [
            {"id": "123", "name": "@everyone", "permissions": "0", "position": 0},
            {"id": "r1", "name": "Member", "color": 0, "permissions": "0", "position": 1}
        ],
        "channels": [{
            "id": "c1", "name": "Voice", "type": 2, "position": 0, "parent_id": null,
            "bitrate": 8000, "user_limit": 0, "rate_limit_per_user": 0, "topic": null,
            "permission_overwrites": []
        }],
        "emojis": []
    }))
    .expect("literal fixture")
}

fn assert_refused(cases: &[(&str, &str)], invalid_values: &[Value]) {
    let mut failures = Vec::new();
    for (pointer, path) in cases {
        for invalid in invalid_values {
            for target_invalid in [false, true] {
                for sealed in [false, true] {
                    let mut source = Value::Object(snapshot());
                    let mut target = Value::Object(snapshot());
                    let (input, label) = if target_invalid {
                        (&mut target, "Target")
                    } else {
                        (&mut source, "Snapshot")
                    };
                    *input.pointer_mut(pointer).expect("fixture field") = invalid.clone();
                    let source = source.as_object().expect("source object").clone();
                    let source = if sealed {
                        let source = seal_snapshot(source);
                        verify_snapshot_integrity(&source).expect("valid seal over invalid scalar");
                        source
                    } else {
                        source
                    };
                    let expected = format!("{label}.{path} must be ");
                    match plan_restore(&source, target.as_object().expect("target object")) {
                        Err(error) if error.0.starts_with(&expected) => {}
                        result => failures.push(format!(
                            "{expected}(invalid={invalid}, sealed={sealed}), got {result:?}"
                        )),
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn blank_guild_strings_refuse_before_emitting_a_plan() {
    assert_refused(
        &[
            ("/guild/name", "guild.name"),
            ("/guild/preferred_locale", "guild.preferred_locale"),
        ],
        &[json!(""), json!(" \t\r\n"), json!("\u{2003}")],
    );
}

#[test]
fn negative_discord_scalars_refuse_before_emitting_a_plan() {
    assert_refused(
        &[
            ("/guild/verification_level", "guild.verification_level"),
            (
                "/guild/default_message_notifications",
                "guild.default_message_notifications",
            ),
            (
                "/guild/explicit_content_filter",
                "guild.explicit_content_filter",
            ),
            ("/guild/afk_timeout", "guild.afk_timeout"),
            ("/guild/system_channel_flags", "guild.system_channel_flags"),
            ("/roles/1/color", "roles[1].color"),
            ("/channels/0/bitrate", "channels[0].bitrate"),
            ("/channels/0/user_limit", "channels[0].user_limit"),
            (
                "/channels/0/rate_limit_per_user",
                "channels[0].rate_limit_per_user",
            ),
        ],
        &[json!(-1), json!(i64::MIN)],
    );
}

#[test]
fn valid_scalars_and_nullable_fields_still_plan_without_changes() {
    let current = snapshot();
    for source in [current.clone(), seal_snapshot(current.clone())] {
        let plan = plan_restore(&source, &current).expect("valid boundary scalars");
        assert!(plan.operations.is_empty());
        assert_eq!(plan.counts.operations, 0);
    }
}

#[test]
fn omitted_optional_scalars_still_plan_without_changes() {
    let mut current = snapshot();
    let guild = current["guild"].as_object_mut().expect("guild object");
    for field in [
        "name",
        "preferred_locale",
        "verification_level",
        "default_message_notifications",
        "explicit_content_filter",
        "afk_timeout",
        "system_channel_flags",
    ] {
        guild.remove(field);
    }
    current["roles"][1]
        .as_object_mut()
        .expect("role object")
        .remove("color");
    let channel = current["channels"][0]
        .as_object_mut()
        .expect("channel object");
    for field in ["bitrate", "user_limit", "rate_limit_per_user"] {
        channel.remove(field);
    }
    for source in [current.clone(), seal_snapshot(current.clone())] {
        let plan = plan_restore(&source, &current).expect("optional fields may be absent");
        assert!(plan.operations.is_empty());
        assert_eq!(plan.counts.operations, 0);
    }
}
