//! Acceptance tests: join-risk and anti-nuke thresholds are hot-wired.
//!
//! Web-dashboard parity spec section 7 slice 2a: the five tuning thresholds
//! below must live in `HOT_WIRED` so a stored `guild_settings` row applies at
//! runtime without a restart. Today they are `Hot` but unwired: the refresh
//! report files them as cold ("next restart"), the live snapshot filters them
//! out, and only each runtime's private store poll picks them up. The raid
//! thresholds (`TWO_RAID_JOIN_THRESHOLD`, `TWO_RAID_WINDOW_SECONDS`) are the
//! precedent: `Hot`, wired, and store-read at once.
//!
//! The builder must preserve this file verbatim (see the manifest on the
//! acceptance card). The `threshold_` tests use only today's public
//! `two_bot_core::settings` API and fail because the five keys are not yet
//! listed in `HOT_WIRED`. No new runtime seam is contracted: both runtimes
//! already read these keys store-first per observation
//! (`join_risk_runtime.rs` `HOT_KEYS`, `containment_runtime.rs` `HOT_KEYS`)
//! and refuse malformed rows instead of falling back, so listing the keys is
//! the whole behavior change. The builder also updates the count tripwire in
//! `settings.rs`, the refresh-partition pins that assume an unwired hot key
//! exists, and the regenerated `docs/configuration.md` labels. The
//! `non_threshold_` and `control_` tests use only today's API, pass before
//! the change, and must stay green after it.
//!
//! Out of scope on purpose: `TWO_BULK_JOIN_WINDOW_UNTIL` is join-risk-adjacent
//! but outside the `TWO_JOIN_RISK_*` / `TWO_ANTI_NUKE_*` threshold slice, so
//! no test here requires it to be wired.

use serde_json::{json, Value};
use two_bot_core::settings::{
    classify_key, live_channel, validate_write, SettingClass, SettingRow, SettingsSnapshot,
    WriteAction, HOT_WIRED,
};

/// The slice: every `TWO_JOIN_RISK_*` and `TWO_ANTI_NUKE_*` threshold key.
const THRESHOLD_KEYS: [&str; 5] = [
    "TWO_JOIN_RISK_THRESHOLD",
    "TWO_JOIN_RISK_WINDOW_SECONDS",
    "TWO_ANTI_NUKE_WINDOW_SECONDS",
    "TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS",
    "TWO_ANTI_NUKE_HEAT_THRESHOLD",
];

/// One realistic stored value per threshold key: numbers, as the dashboard
/// writes them, inside the range each runtime's parser accepts.
fn stored_value(key: &str) -> Value {
    match key {
        "TWO_JOIN_RISK_THRESHOLD" => json!(8),
        "TWO_JOIN_RISK_WINDOW_SECONDS" => json!(30.5),
        "TWO_ANTI_NUKE_WINDOW_SECONDS" => json!(45),
        "TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS" => json!(90),
        "TWO_ANTI_NUKE_HEAT_THRESHOLD" => json!(12),
        _ => panic!("slice value missing for {key}"),
    }
}

/// The environment rendering each stored value must produce on the live path.
fn rendered_value(key: &str) -> &'static str {
    match key {
        "TWO_JOIN_RISK_THRESHOLD" => "8",
        "TWO_JOIN_RISK_WINDOW_SECONDS" => "30.5",
        "TWO_ANTI_NUKE_WINDOW_SECONDS" => "45",
        "TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS" => "90",
        "TWO_ANTI_NUKE_HEAT_THRESHOLD" => "12",
        _ => panic!("slice rendering missing for {key}"),
    }
}

const GUILD: &str = "acceptance-guild";
const OTHER_GUILD: &str = "other-acceptance-guild";
const ACTOR: &str = "acceptance-actor";

fn row(guild: &str, key: &str, value: Value) -> SettingRow {
    SettingRow {
        guild_id: guild.to_owned(),
        key: key.to_owned(),
        value,
        version: 1,
    }
}

fn threshold_rows(guild: &str) -> Vec<SettingRow> {
    THRESHOLD_KEYS
        .iter()
        .map(|key| row(guild, key, stored_value(key)))
        .collect()
}

#[test]
fn threshold_keys_are_classified_hot_and_hot_wired() {
    for key in THRESHOLD_KEYS {
        assert_eq!(
            classify_key(key),
            Some(SettingClass::Hot),
            "a threshold that applies without a restart must be Hot: {key}"
        );
        assert!(
            HOT_WIRED.contains(&key),
            "live readers only see hot-wired keys; {key} must be listed"
        );
        let write = validate_write(GUILD, key, Some(stored_value(key)), ACTOR)
            .unwrap_or_else(|_| panic!("a dashboard threshold must be storable: {key}"));
        assert_eq!(write.class, SettingClass::Hot, "{key}");
        assert_eq!(
            write.action,
            WriteAction::Upsert(stored_value(key)),
            "{key}"
        );
    }
}

#[test]
fn threshold_rows_report_hot_not_cold() {
    let (mut writer, _reader) = live_channel();
    let report = writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: threshold_rows(GUILD),
    });
    assert!(report.changed, "the poll moved: {report:?}");
    assert_eq!(report.hot.len(), THRESHOLD_KEYS.len(), "{report:?}");
    assert!(report.cold.is_empty(), "{report:?}");
    assert!(report.ignored.is_empty(), "{report:?}");
    for key in THRESHOLD_KEYS {
        let change = report
            .hot
            .iter()
            .find(|change| change.key == key)
            .unwrap_or_else(|| panic!("missing hot change for {key}: {report:?}"));
        assert_eq!(change.guild_id, GUILD, "{key}");
        assert_eq!(change.old, None, "{key}");
        assert_eq!(
            change.new.as_deref(),
            Some(rendered_value(key)),
            "the live report must carry the env rendering for {key}"
        );
    }
}

#[test]
fn published_snapshot_serves_thresholds_live_per_guild() {
    let (mut writer, reader) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: threshold_rows(GUILD),
    });
    for key in THRESHOLD_KEYS {
        assert_eq!(
            reader.get(GUILD, key),
            Some(stored_value(key)),
            "the live snapshot must serve the stored {key} for its guild"
        );
        assert_eq!(
            reader.get(OTHER_GUILD, key),
            None,
            "one guild's stored {key} never leaks to another guild"
        );
    }
    let rendered = reader.env_snapshot(Some(GUILD));
    for key in THRESHOLD_KEYS {
        assert_eq!(
            rendered.get(key).map(String::as_str),
            Some(rendered_value(key)),
            "the env-shaped view must carry the stored {key}"
        );
    }
    assert!(reader.env_snapshot(None).is_empty());
    assert!(reader.env_snapshot(Some(OTHER_GUILD)).is_empty());
}

#[test]
fn deleting_a_threshold_row_reports_a_hot_change_back_to_env() {
    let (mut writer, reader) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: threshold_rows(GUILD),
    });
    for key in THRESHOLD_KEYS {
        assert!(
            reader.get(GUILD, key).is_some(),
            "precondition: {key} is served live before the delete"
        );
    }
    let narrowed = SettingsSnapshot {
        revision: 2,
        rows: Vec::new(),
    };
    assert!(writer.needs_refresh(2, 0));
    let report = writer.publish(&narrowed);
    assert!(report.changed, "the delete moved the poll: {report:?}");
    assert_eq!(report.hot.len(), THRESHOLD_KEYS.len(), "{report:?}");
    assert!(report.cold.is_empty(), "{report:?}");
    for key in THRESHOLD_KEYS {
        let change = report
            .hot
            .iter()
            .find(|change| change.key == key)
            .unwrap_or_else(|| panic!("missing hot delete for {key}: {report:?}"));
        assert_eq!(change.old.as_deref(), Some(rendered_value(key)), "{key}");
        assert_eq!(change.new, None, "{key} hands back to the environment");
        assert_eq!(
            reader.get(GUILD, key),
            None,
            "{key} reads from the environment again after the delete"
        );
    }
}

/// The non-threshold anti-nuke keys stay out of the live set: the enable and
/// dry-run gates are boot-read cold, and the trusted/protected lists plus the
/// snapshot path are environment-only reach boundaries. Wiring any of them
/// would let a dashboard write flip arming or widen who containment ignores.
#[test]
fn non_threshold_anti_nuke_keys_stay_out_of_the_live_set() {
    for key in ["TWO_ANTI_NUKE", "TWO_ANTI_NUKE_DRY_RUN"] {
        assert_eq!(classify_key(key), Some(SettingClass::Cold), "{key}");
        assert!(!HOT_WIRED.contains(&key), "{key} must stay boot-read");
    }
    for key in [
        "TWO_ANTI_NUKE_TRUSTED_USER_IDS",
        "TWO_ANTI_NUKE_PROTECTED_USER_IDS",
        "TWO_ANTI_NUKE_SNAPSHOT_PATH",
    ] {
        assert_eq!(classify_key(key), Some(SettingClass::EnvOnly), "{key}");
        assert!(
            !HOT_WIRED.contains(&key),
            "{key} must stay environment-only"
        );
        assert!(
            validate_write(GUILD, key, Some(json!("synthetic")), ACTOR).is_err(),
            "{key} must refuse dashboard writes"
        );
    }

    let (mut writer, reader) = live_channel();
    let report = writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![
            row(GUILD, "TWO_ANTI_NUKE", json!("1")),
            row(GUILD, "TWO_ANTI_NUKE_DRY_RUN", json!("0")),
            row(GUILD, "TWO_ANTI_NUKE_TRUSTED_USER_IDS", json!("synthetic")),
        ],
    });
    assert!(report.hot.is_empty(), "{report:?}");
    for key in ["TWO_ANTI_NUKE", "TWO_ANTI_NUKE_DRY_RUN"] {
        assert!(
            report.cold.iter().any(|change| change.key == key),
            "boot-read {key} reports cold: {report:?}"
        );
        assert_eq!(reader.get(GUILD, key), None, "{key} is never served live");
    }
    assert!(
        report
            .ignored
            .iter()
            .any(|ignored| ignored.key == "TWO_ANTI_NUKE_TRUSTED_USER_IDS"),
        "env-only rows are ignored, never applied: {report:?}"
    );
    assert_eq!(reader.get(GUILD, "TWO_ANTI_NUKE_TRUSTED_USER_IDS"), None);
}

/// Control: the raid thresholds are the already-wired precedent. They pass
/// today and must stay green: the slice adds keys to the live set, it never
/// removes or re-partitions the ones already there.
#[test]
fn control_raid_thresholds_stay_hot_wired() {
    for key in ["TWO_RAID_JOIN_THRESHOLD", "TWO_RAID_WINDOW_SECONDS"] {
        assert_eq!(classify_key(key), Some(SettingClass::Hot), "{key}");
        assert!(HOT_WIRED.contains(&key), "{key}");
    }
    let (mut writer, reader) = live_channel();
    let report = writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![
            row(GUILD, "TWO_RAID_JOIN_THRESHOLD", json!(5)),
            row(GUILD, "TWO_RAID_WINDOW_SECONDS", json!(60)),
        ],
    });
    assert_eq!(report.hot.len(), 2, "{report:?}");
    assert!(report.cold.is_empty(), "{report:?}");
    assert_eq!(reader.get(GUILD, "TWO_RAID_JOIN_THRESHOLD"), Some(json!(5)));
    assert_eq!(
        reader.get(GUILD, "TWO_RAID_WINDOW_SECONDS"),
        Some(json!(60))
    );
}
