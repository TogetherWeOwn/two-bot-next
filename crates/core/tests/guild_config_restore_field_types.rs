//! Offline acceptance for Discord-bound restore field types. No transport or database.
use serde_json::{json, Map, Value};
use two_bot_core::backup::{
    guild_config::{seal_snapshot, verify_snapshot_integrity},
    guild_config_restore::{plan_restore, RestorePath},
};

fn snapshot() -> Map<String, Value> {
    serde_json::from_value(json!({
        "version": 1,
        "guildId": "1545644954272137297",
        "guild": {
            "name": "Guild",
            "description": "A community",
            "verification_level": 2,
            "default_message_notifications": 0,
            "explicit_content_filter": 2,
            "afk_timeout": 300,
            "preferred_locale": "en-US",
            "premium_progress_bar_enabled": false,
            "system_channel_flags": 0,
            "system_channel_id": null,
            "rules_channel_id": null,
            "public_updates_channel_id": null,
            "afk_channel_id": null
        },
        "roles": [
            {"id": "1545644954272137297", "name": "@everyone", "managed": false,
             "color": 0, "hoist": false, "permissions": "0", "mentionable": false, "position": 0},
            {"id": "r1", "name": "Member", "managed": false,
             "color": 255, "hoist": true, "permissions": "1024", "mentionable": false, "position": 1}
        ],
        "channels": [
            {"id": "c1", "name": "general", "type": 0, "parent_id": null, "position": 0,
             "topic": "Welcome", "nsfw": false, "rate_limit_per_user": 5, "permission_overwrites": []},
            {"id": "c2", "name": "Voice", "type": 2, "parent_id": null, "position": 1,
             "bitrate": 64000, "user_limit": 10, "nsfw": false, "permission_overwrites": []}
        ],
        "emojis": []
    }))
    .expect("literal snapshot")
}

fn non_integers() -> Vec<Value> {
    vec![
        json!("300"),
        json!(false),
        json!(1.5),
        json!([]),
        json!({}),
        Value::Null,
    ]
}

fn non_strings() -> Vec<Value> {
    vec![json!(7), json!(false), json!([]), json!({})]
}

fn non_booleans() -> Vec<Value> {
    vec![json!("false"), json!(0), json!([]), json!({}), Value::Null]
}

fn assert_snapshot_field_refused(pointer: &str, path: &str, invalid_values: Vec<Value>) {
    let current = snapshot();
    let mut failures = Vec::new();
    for invalid in invalid_values {
        for sealed in [false, true] {
            let mut source = Value::Object(snapshot());
            *source.pointer_mut(pointer).expect("fixture field exists") = invalid.clone();
            let source = source.as_object().expect("snapshot object").clone();
            let source = if sealed {
                seal_snapshot(source)
            } else {
                source
            };
            if sealed {
                verify_snapshot_integrity(&source).expect("valid seal over mistyped input");
            }
            // A returned plan is already too late: its earlier operations could
            // succeed before Discord rejects the mistyped field. Err exposes no
            // executable operations at all, including for correctly sealed input.
            match plan_restore(&source, &current) {
                Ok(plan) => failures.push(format!(
                    "{path}={invalid} (sealed={sealed}): emitted {} operations instead of refusing",
                    plan.operations.len()
                )),
                Err(error) => {
                    if !error.0.starts_with(&format!("{path} must be ")) {
                        failures.push(format!(
                            "{path}={invalid} (sealed={sealed}): expected shape_error naming path, got {error}"
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

macro_rules! refuses_field {
    ($test:ident, $pointer:literal, $path:literal, $invalid:expr) => {
        #[test]
        fn $test() {
            assert_snapshot_field_refused($pointer, $path, $invalid);
        }
    };
}

refuses_field!(
    role_permissions_require_a_decimal_mask,
    "/roles/1/permissions",
    "Snapshot.roles[1].permissions",
    vec![
        json!("not-a-mask"),
        json!("0x8"),
        json!("-1"),
        json!("8.0"),
        json!(" 8"),
        json!(""),
        json!(8),
        json!(false),
        json!([]),
        json!({}),
        Value::Null
    ]
);
refuses_field!(
    role_color_requires_an_integer,
    "/roles/1/color",
    "Snapshot.roles[1].color",
    non_integers()
);
refuses_field!(
    channel_bitrate_requires_an_integer,
    "/channels/1/bitrate",
    "Snapshot.channels[1].bitrate",
    non_integers()
);
refuses_field!(
    channel_user_limit_requires_an_integer,
    "/channels/1/user_limit",
    "Snapshot.channels[1].user_limit",
    non_integers()
);
refuses_field!(
    channel_rate_limit_requires_an_integer,
    "/channels/0/rate_limit_per_user",
    "Snapshot.channels[0].rate_limit_per_user",
    non_integers()
);
refuses_field!(
    channel_topic_requires_a_string_or_null,
    "/channels/0/topic",
    "Snapshot.channels[0].topic",
    non_strings()
);
refuses_field!(
    channel_name_requires_a_string,
    "/channels/0/name",
    "Snapshot.channels[0].name",
    non_strings()
);
refuses_field!(
    channel_nsfw_requires_a_boolean,
    "/channels/0/nsfw",
    "Snapshot.channels[0].nsfw",
    non_booleans()
);
refuses_field!(
    guild_name_requires_a_string,
    "/guild/name",
    "Snapshot.guild.name",
    non_strings()
);
refuses_field!(
    guild_description_requires_a_string_or_null,
    "/guild/description",
    "Snapshot.guild.description",
    non_strings()
);
refuses_field!(
    guild_verification_level_requires_an_integer,
    "/guild/verification_level",
    "Snapshot.guild.verification_level",
    non_integers()
);
refuses_field!(
    guild_default_notifications_require_an_integer,
    "/guild/default_message_notifications",
    "Snapshot.guild.default_message_notifications",
    non_integers()
);
refuses_field!(
    guild_explicit_content_filter_requires_an_integer,
    "/guild/explicit_content_filter",
    "Snapshot.guild.explicit_content_filter",
    non_integers()
);
refuses_field!(
    guild_afk_timeout_requires_an_integer,
    "/guild/afk_timeout",
    "Snapshot.guild.afk_timeout",
    non_integers()
);
refuses_field!(
    guild_preferred_locale_requires_a_string,
    "/guild/preferred_locale",
    "Snapshot.guild.preferred_locale",
    non_strings()
);
refuses_field!(
    guild_progress_bar_requires_a_boolean,
    "/guild/premium_progress_bar_enabled",
    "Snapshot.guild.premium_progress_bar_enabled",
    non_booleans()
);
refuses_field!(
    guild_system_channel_flags_require_an_integer,
    "/guild/system_channel_flags",
    "Snapshot.guild.system_channel_flags",
    non_integers()
);

#[test]
fn well_formed_identical_snapshots_emit_no_operations() {
    for sealed in [false, true] {
        let source = if sealed {
            seal_snapshot(snapshot())
        } else {
            snapshot()
        };
        let plan = plan_restore(&source, &snapshot()).expect("well-formed unchanged snapshot");
        assert!(plan.operations.is_empty());
        assert_eq!(plan.counts.operations, 0);
    }
}

#[test]
fn null_descriptions_topics_and_omitted_channel_specific_fields_remain_valid() {
    let mut source = snapshot();
    source["guild"]["description"] = Value::Null;
    source["channels"][0]["topic"] = Value::Null;
    let current = source.clone();
    let plan = plan_restore(&seal_snapshot(source), &current).expect("nullable fields are valid");
    assert!(plan.operations.is_empty());
    assert_eq!(plan.counts.operations, 0);
}

#[test]
fn well_formed_patch_plan_remains_byte_identical() {
    let source = seal_snapshot(snapshot());
    let mut current = snapshot();
    current["roles"][1]["color"] = json!(0);
    current["channels"][0]["topic"] = json!("Old topic");
    current["guild"]["afk_timeout"] = json!(60);
    let plan = plan_restore(&source, &current).expect("valid scalar drift");
    assert_eq!(plan.counts.operations, 3);
    let actual: Vec<Value> = plan
        .operations
        .iter()
        .map(|operation| {
            let path = match &operation.path {
                RestorePath::Literal(path) => path.clone(),
                RestorePath::Channel(id) => format!("/channels/{id}"),
            };
            json!({"method": operation.method, "path": path,
               "body": operation.body, "capture_id": operation.capture_id})
        })
        .collect();
    let expected = json!([
        {"method": "PATCH", "path": "/guilds/1545644954272137297/roles/r1",
         "body": {"name": "Member", "color": 255, "hoist": true,
                  "permissions": "1024", "mentionable": false}, "capture_id": null},
        {"method": "PATCH", "path": "/channels/c1",
         "body": {"name": "general", "topic": "Welcome", "nsfw": false,
                  "rate_limit_per_user": 5, "type": 0, "parent_id": null}, "capture_id": null},
        {"method": "PATCH", "path": "/guilds/1545644954272137297",
         "body": {"name": "Guild", "description": "A community", "verification_level": 2,
                  "default_message_notifications": 0, "explicit_content_filter": 2,
                  "afk_timeout": 300, "preferred_locale": "en-US",
                  "premium_progress_bar_enabled": false, "system_channel_flags": 0,
                  "system_channel_id": null, "rules_channel_id": null,
                  "public_updates_channel_id": null, "afk_channel_id": null}, "capture_id": null}
    ]);
    assert_eq!(
        serde_json::to_vec(&actual).expect("operation bytes"),
        serde_json::to_vec(&expected).expect("literal golden bytes"),
        "valid restore payloads must not change when adding shape validation"
    );
}
