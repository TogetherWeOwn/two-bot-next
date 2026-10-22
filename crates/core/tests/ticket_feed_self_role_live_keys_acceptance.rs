//! Protected acceptance: ticket, feed and self-role keys go live.
//!
//! PROTECTED FILE: the paired Builder reclassifies the three keys, wires
//! their consumers to the live snapshot, and adds per-key write validation
//! without editing this file. Return necessary corrections to the Test
//! Author; never edit.
//!
//! Contract under test (all through today's public
//! `two_bot_core::settings` API plus the existing `FeatureGates` /
//! `SelfRoleGates` loaders — pure in-memory via `live_channel`, no
//! database, no network, no process-environment reads):
//!
//! - The trio (`TWO_TICKET_COOLDOWN_SECONDS`, `TWO_ANNOUNCEMENTS`,
//!   `TWO_SELF_ROLE_PANELS`) classifies `Hot`, joins `HOT_WIRED`, reports
//!   hot (never cold) with env-string from/to on a live poll, and serves
//!   live per guild with no cross-guild leak.
//! - Stored rows override boot env store-first (the TOG-19027 pattern):
//!   layering the live `env_snapshot` over boot vars flips the effective
//!   cooldown, the announcements gate (both directions), and the parsed
//!   self-role catalogue; deleting a row reports a hot change back to
//!   boot (`new` = `None`) and the layered map carries boot again.
//! - Invalid writes are refused before any SQL (zero mutation): the
//!   announcements gate takes only boolean/0/1 with the existing typed
//!   `InvalidGateValue` code; cooldown and panels refusals name the key
//!   in both the typed code and the next-action message.
//! - B5 exclusion keys (reach/identity: moderation master, protected
//!   IDs, Owen ID, `TWO_INTERNAL_*`, assistant/synthetic IDs) stay
//!   `EnvOnly`, refuse writes, and are ignored (never served) on
//!   refresh; B7 honest gaps (warn/strike escalation, mute-role config)
//!   stay uninvented — no catalogue key claims those semantics.
//! - Fallback premises stay safe: absent rows read back as unset, the
//!   catalogue loader refuses malformed input without panicking, and
//!   only exactly `"1"` enables the announcements gate (fail-closed).

use std::collections::HashMap;

use serde_json::{json, Value};
use two_bot_core::settings::{
    classify_key, is_storable_key, live_channel, to_env_string, validate_write, KeyChange,
    RefreshReport, SettingClass, SettingRow, SettingsSnapshot, WriteAction, WriteRefusal,
    ENV_ONLY_KEY_PREFIXES, HOT_WIRED, SETTING_CLASSES,
};
use two_bot_core::{parse_self_role_panels, FeatureGates, SelfRoleGates};

const COOLDOWN: &str = "TWO_TICKET_COOLDOWN_SECONDS";
const ANNOUNCEMENTS: &str = "TWO_ANNOUNCEMENTS";
const PANELS: &str = "TWO_SELF_ROLE_PANELS";
const TRIO: [&str; 3] = [COOLDOWN, ANNOUNCEMENTS, PANELS];

const GUILD: &str = "acceptance-guild";
const OTHER_GUILD: &str = "other-acceptance-guild";
const ACTOR: &str = "acceptance-actor";

/// B5 PUT-allowlist exclusions: reach/identity keys that stay env-only.
const B5_EXCLUSIONS: &[&str] = &[
    "TWO_MODERATION",
    "TWO_MODERATION_PROTECTED_ROLE_IDS",
    "TWO_OWEN_USER_ID",
    "TWO_INTERNAL_ACTIONS",
    "TWO_ASSISTANT_ENDPOINT",
    "TWO_ASSISTANT_MODEL",
    "TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS",
    "TWO_ANTI_NUKE_PROTECTED_USER_IDS",
    "TWO_ANTI_NUKE_TRUSTED_USER_IDS",
];

fn row(guild: &str, key: &str, value: Value, version: i64) -> SettingRow {
    SettingRow {
        guild_id: guild.to_owned(),
        key: key.to_owned(),
        value,
        version,
    }
}

fn snapshot(revision: i64, rows: Vec<SettingRow>) -> SettingsSnapshot {
    SettingsSnapshot { revision, rows }
}

fn hot_change<'a>(report: &'a RefreshReport, key: &str) -> &'a KeyChange {
    report
        .hot
        .iter()
        .find(|change| change.key == key)
        .unwrap_or_else(|| panic!("missing hot change for {key}: {report:?}"))
}

/// Minimal valid catalogue: one button panel, one option, house snowflakes.
fn panels_value() -> Value {
    json!([{
        "id": "games",
        "channelId": "111111111111111111",
        "messageId": "555555555555555555",
        "mode": "button",
        "options": [{
            "key": "chess",
            "label": "Chess",
            "roleId": "222222222222222222",
            "permissions": "0"
        }]
    }])
}

/// TOG-19027 store-first composition for one key: the live snapshot wins,
/// an absent row falls back to boot.
fn layer_key(
    boot: &HashMap<String, String>,
    live: &HashMap<String, String>,
    key: &str,
) -> HashMap<String, String> {
    let mut vars = boot.clone();
    if let Some(value) = live.get(key) {
        vars.insert(key.to_owned(), value.clone());
    }
    vars
}

fn boot_vars() -> HashMap<String, String> {
    [(COOLDOWN, "300"), (ANNOUNCEMENTS, "0")]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

// ---------------------------------------------------------------------------
// (1) classification: the trio is Hot and hot-wired.
// ---------------------------------------------------------------------------

#[test]
fn trio_keys_are_classified_hot_and_hot_wired() {
    for key in TRIO {
        assert_eq!(classify_key(key), Some(SettingClass::Hot), "{key}");
        assert!(HOT_WIRED.contains(&key), "{key}");
        assert!(is_storable_key(key), "{key}");
    }
}

// ---------------------------------------------------------------------------
// (2) live poll: trio rows report hot with env renderings and serve live.
// ---------------------------------------------------------------------------

#[test]
fn trio_rows_report_hot_with_env_renderings_on_live_poll() {
    let panels = panels_value();
    let panels_rendered = to_env_string(&panels).expect("panels render");
    let (mut writer, live) = live_channel();
    let report = writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, COOLDOWN, json!(600), 1),
            row(GUILD, ANNOUNCEMENTS, json!(true), 2),
            row(GUILD, PANELS, panels.clone(), 3),
        ],
    ));
    assert!(report.changed);
    assert_eq!(report.hot.len(), 3, "{report:?}");
    for key in TRIO {
        assert!(
            report.hot.iter().any(|change| change.key == key),
            "missing {key}: {report:?}"
        );
        assert!(
            report.cold.iter().all(|change| change.key != key),
            "{key} must not report cold: {report:?}"
        );
    }
    // The setting_changed payload per key: from unset, to env rendering.
    let change = hot_change(&report, COOLDOWN);
    assert_eq!(change.guild_id, GUILD);
    assert_eq!(change.old, None);
    assert_eq!(change.new.as_deref(), Some("600"));
    let change = hot_change(&report, ANNOUNCEMENTS);
    assert_eq!(change.old, None);
    assert_eq!(change.new.as_deref(), Some("1"));
    let change = hot_change(&report, PANELS);
    assert_eq!(change.old, None);
    assert_eq!(change.new.as_deref(), Some(panels_rendered.as_str()));
    // The panels rendering round-trips through the catalogue loader.
    assert_eq!(
        parse_self_role_panels(&panels_rendered).expect("rendering parses"),
        parse_self_role_panels(&panels.to_string()).expect("fixture parses")
    );
    // Live serve per guild, with no cross-guild leak.
    assert_eq!(live.get(GUILD, COOLDOWN), Some(json!(600)));
    assert_eq!(live.get(GUILD, ANNOUNCEMENTS), Some(json!(true)));
    assert_eq!(live.get(GUILD, PANELS), Some(panels));
    for key in TRIO {
        assert_eq!(live.get(OTHER_GUILD, key), None, "{key}");
    }
    let rendered = live.env_snapshot(Some(GUILD));
    assert_eq!(rendered.get(COOLDOWN).map(String::as_str), Some("600"));
    assert_eq!(rendered.get(ANNOUNCEMENTS).map(String::as_str), Some("1"));
    assert_eq!(
        rendered.get(PANELS).map(String::as_str),
        Some(panels_rendered.as_str())
    );
    let other = live.env_snapshot(Some(OTHER_GUILD));
    for key in TRIO {
        assert!(!other.contains_key(key), "{key}");
    }
}

// ---------------------------------------------------------------------------
// (3) store-first: stored rows override boot env through the real loaders.
// ---------------------------------------------------------------------------

#[test]
fn trio_stored_rows_override_boot_env_store_first() {
    let panels = panels_value();
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, COOLDOWN, json!(600), 1),
            row(GUILD, ANNOUNCEMENTS, json!(true), 2),
            row(GUILD, PANELS, panels.clone(), 3),
        ],
    ));
    let snap = live.env_snapshot(Some(GUILD));
    let boot = boot_vars();

    // Boot says 300/off/empty; the stored rows win per key.
    let vars = layer_key(&boot, &snap, COOLDOWN);
    assert_eq!(vars.get(COOLDOWN).map(String::as_str), Some("600"));
    let vars = layer_key(&boot, &snap, ANNOUNCEMENTS);
    assert!(
        FeatureGates::from_map(&vars)
            .expect("gates parse")
            .announcements
    );
    let vars = layer_key(&boot, &snap, PANELS);
    assert_eq!(
        SelfRoleGates::from_map(&vars).expect("panels parse").panels,
        parse_self_role_panels(&panels.to_string()).expect("fixture parses")
    );
    // Boot alone carries the old behavior, proving the override direction.
    assert!(SelfRoleGates::from_map(&boot)
        .expect("boot parses")
        .panels
        .is_empty());
    assert!(
        !FeatureGates::from_map(&boot)
            .expect("boot parses")
            .announcements
    );

    // The reverse direction: a stored "0" disables over a boot "1".
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![row(GUILD, ANNOUNCEMENTS, json!(false), 1)],
    ));
    let snap = live.env_snapshot(Some(GUILD));
    let boot_on: HashMap<String, String> = [(ANNOUNCEMENTS.to_owned(), "1".to_owned())]
        .into_iter()
        .collect();
    let vars = layer_key(&boot_on, &snap, ANNOUNCEMENTS);
    assert!(
        !FeatureGates::from_map(&vars)
            .expect("gates parse")
            .announcements
    );
}

// ---------------------------------------------------------------------------
// (4) delete: rows report hot changes back to boot and fall back.
// ---------------------------------------------------------------------------

#[test]
fn trio_deletes_report_hot_changes_back_to_boot() {
    let panels = panels_value();
    let panels_rendered = to_env_string(&panels).expect("panels render");
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, COOLDOWN, json!(600), 1),
            row(GUILD, ANNOUNCEMENTS, json!(true), 2),
            row(GUILD, PANELS, panels, 3),
        ],
    ));
    let report = writer.publish(&snapshot(2, vec![]));
    assert!(report.changed);
    assert_eq!(report.hot.len(), 3, "{report:?}");
    for (key, old) in [
        (COOLDOWN, "600"),
        (ANNOUNCEMENTS, "1"),
        (PANELS, panels_rendered.as_str()),
    ] {
        let change = hot_change(&report, key);
        assert_eq!(change.old.as_deref(), Some(old), "{key}");
        assert_eq!(change.new, None, "{key}");
        assert_eq!(live.get(GUILD, key), None, "{key}");
    }
    // Absent rows fall back: the layered map carries boot again.
    let snap = live.env_snapshot(Some(GUILD));
    assert!(snap.keys().all(|key| !TRIO.contains(&key.as_str())));
    let boot = boot_vars();
    let vars = layer_key(&boot, &snap, COOLDOWN);
    assert_eq!(vars.get(COOLDOWN).map(String::as_str), Some("300"));
    let vars = layer_key(&boot, &snap, ANNOUNCEMENTS);
    assert!(
        !FeatureGates::from_map(&vars)
            .expect("gates parse")
            .announcements
    );
}

// ---------------------------------------------------------------------------
// (5) writes: invalid values refused with a typed code before any SQL.
// ---------------------------------------------------------------------------

#[test]
fn announcements_invalid_writes_refused_with_typed_gate_code() {
    for value in [json!("on"), json!("true"), json!(""), json!(2), json!(2.5)] {
        assert_eq!(
            validate_write(GUILD, ANNOUNCEMENTS, Some(value.clone()), ACTOR),
            Err(WriteRefusal::InvalidGateValue(ANNOUNCEMENTS.to_owned())),
            "{value}"
        );
    }
    for value in [
        json!(true),
        json!(false),
        json!(1),
        json!(0),
        json!("1"),
        json!("0"),
    ] {
        let write = validate_write(GUILD, ANNOUNCEMENTS, Some(value.clone()), ACTOR)
            .unwrap_or_else(|err| panic!("valid gate refused {value}: {err:?}"));
        assert_eq!(write.class, SettingClass::Hot, "{value}");
        assert_eq!(write.action, WriteAction::Upsert(value));
    }
    assert_eq!(
        validate_write(GUILD, ANNOUNCEMENTS, None, ACTOR)
            .expect("delete validates")
            .action,
        WriteAction::Delete
    );
    assert_eq!(
        validate_write(GUILD, ANNOUNCEMENTS, Some(json!(true)), ""),
        Err(WriteRefusal::MissingActor)
    );
}

#[test]
fn cooldown_invalid_writes_refused_naming_key() {
    for value in [json!("never"), json!(-5), json!({"value": 600})] {
        let refusal = format!("invalid cooldown write refused: {value}");
        let err = validate_write(GUILD, COOLDOWN, Some(value.clone()), ACTOR).expect_err(&refusal);
        assert!(
            format!("{err:?}").contains(COOLDOWN),
            "typed code names the key: {err:?}"
        );
        assert!(
            err.to_string().contains(COOLDOWN),
            "next action names the key: {err}"
        );
    }
    for value in [json!(600), json!("600")] {
        let write = validate_write(GUILD, COOLDOWN, Some(value.clone()), ACTOR)
            .unwrap_or_else(|err| panic!("valid cooldown refused {value}: {err:?}"));
        assert_eq!(write.class, SettingClass::Hot, "{value}");
        assert_eq!(write.action, WriteAction::Upsert(value));
    }
    assert_eq!(
        validate_write(GUILD, COOLDOWN, None, ACTOR)
            .expect("delete validates")
            .action,
        WriteAction::Delete
    );
    assert_eq!(
        validate_write(GUILD, COOLDOWN, Some(json!(600)), ""),
        Err(WriteRefusal::MissingActor)
    );
}

#[test]
fn panels_invalid_writes_refused_naming_key() {
    for value in [json!("not-json"), json!({"panels": []})] {
        let refusal = format!("invalid panels write refused: {value}");
        let err = validate_write(GUILD, PANELS, Some(value.clone()), ACTOR).expect_err(&refusal);
        assert!(
            format!("{err:?}").contains(PANELS),
            "typed code names the key: {err:?}"
        );
        assert!(
            err.to_string().contains(PANELS),
            "next action names the key: {err}"
        );
    }
    let panels = panels_value();
    let write = validate_write(GUILD, PANELS, Some(panels.clone()), ACTOR)
        .expect("valid catalogue validates");
    assert_eq!(write.class, SettingClass::Hot);
    assert_eq!(write.action, WriteAction::Upsert(panels));
    assert_eq!(
        validate_write(GUILD, PANELS, None, ACTOR)
            .expect("delete validates")
            .action,
        WriteAction::Delete
    );
    assert_eq!(
        validate_write(GUILD, PANELS, Some(panels_value()), ""),
        Err(WriteRefusal::MissingActor)
    );
}

// ---------------------------------------------------------------------------
// (6) B5/B7 guards: exclusion keys untouched, gaps uninvented.
// ---------------------------------------------------------------------------

#[test]
fn b5_exclusion_keys_stay_env_only_and_refuse_writes() {
    for key in B5_EXCLUSIONS {
        assert_eq!(classify_key(key), Some(SettingClass::EnvOnly), "{key}");
        assert!(
            matches!(
                validate_write(GUILD, key, Some(json!(1)), ACTOR),
                Err(WriteRefusal::EnvOnly(_))
            ),
            "{key}"
        );
    }
    // A gate added under the prefix tomorrow refuses the same way.
    let probe = format!("{}FUTURE_ACCEPTANCE_PROBE", ENV_ONLY_KEY_PREFIXES[0]);
    assert!(
        matches!(
            validate_write(GUILD, &probe, Some(json!(1)), ACTOR),
            Err(WriteRefusal::EnvOnly(_))
        ),
        "{probe}"
    );
    // Refresh ignores them; live never serves them.
    let (mut writer, live) = live_channel();
    let report = writer.publish(&snapshot(
        1,
        B5_EXCLUSIONS
            .iter()
            .map(|key| row(GUILD, key, json!("synthetic"), 1))
            .collect(),
    ));
    assert!(report.changed);
    assert!(report.hot.is_empty(), "{report:?}");
    assert!(report.cold.is_empty(), "{report:?}");
    assert_eq!(report.ignored.len(), B5_EXCLUSIONS.len(), "{report:?}");
    for key in B5_EXCLUSIONS {
        assert_eq!(live.get(GUILD, key), None, "{key}");
    }
    assert!(live.env_snapshot(Some(GUILD)).is_empty());
}

#[test]
fn b7_escalation_and_mute_role_gaps_stay_uninvented() {
    // B7 honest gaps: no catalogue key claims warn/strike escalation or
    // mute-role semantics — never invented as unenforced toggles.
    for (key, _) in SETTING_CLASSES {
        assert!(!key.contains("STRIKE"), "{key}");
        assert!(!key.contains("ESCALAT"), "{key}");
        assert!(!key.contains("MUTE"), "{key}");
    }
    for probe in ["UNLISTED_WARN_ESCALATION_PROBE", "UNLISTED_MUTE_ROLE_PROBE"] {
        assert_eq!(classify_key(probe), None, "{probe}");
        assert!(
            matches!(
                validate_write(GUILD, probe, Some(json!(1)), ACTOR),
                Err(WriteRefusal::Unknown(_))
            ),
            "{probe}"
        );
    }
}

// ---------------------------------------------------------------------------
// (7) fallback premises: absent rows and loader refusals stay safe.
// ---------------------------------------------------------------------------

#[test]
fn fallback_premises_absent_rows_and_loader_refusals_stay_safe() {
    // Absent rows read back as unset through every live API.
    let (_writer, live) = live_channel();
    for key in TRIO {
        assert_eq!(live.get(GUILD, key), None, "{key}");
    }
    assert!(live.env_snapshot(Some(GUILD)).is_empty());
    // Unparsable catalogue input is refused by the loader (no panic, no
    // partial panels): the live reader's fallback trigger.
    for raw in ["not-json", "{bad", "{}"] {
        assert!(parse_self_role_panels(raw).is_err(), "{raw}");
    }
    assert_eq!(
        parse_self_role_panels("").expect("empty disables"),
        Vec::new()
    );
    // Announcements premise: only exactly "1" enables; anything else is
    // safely off (fail-closed) without panicking.
    for (raw, expected) in [
        ("1", true),
        ("0", false),
        ("on", false),
        ("true", false),
        ("", false),
        ("2", false),
    ] {
        let vars: HashMap<String, String> = [(ANNOUNCEMENTS.to_owned(), raw.to_owned())]
            .into_iter()
            .collect();
        assert_eq!(
            FeatureGates::from_map(&vars)
                .expect("gates parse")
                .announcements,
            expected,
            "{raw:?}"
        );
    }
    // Cooldown premise: stored numerics render as plain integer text, the
    // shape the live reader parses with boot fallback.
    assert_eq!(to_env_string(&json!(600)).as_deref(), Some("600"));
    assert_eq!(to_env_string(&json!("600")).as_deref(), Some("600"));
    assert_eq!(to_env_string(&json!("never")).as_deref(), Some("never"));
}

// ---------------------------------------------------------------------------
// (8) control: already-wired keys stay hot-wired.
// ---------------------------------------------------------------------------

#[test]
fn control_feed_interval_and_raid_threshold_stay_hot_wired() {
    for key in ["TWO_FEED_POLL_SECONDS", "TWO_RAID_JOIN_THRESHOLD"] {
        assert_eq!(classify_key(key), Some(SettingClass::Hot), "{key}");
        assert!(HOT_WIRED.contains(&key), "{key}");
    }
    let (mut writer, live) = live_channel();
    let report = writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, "TWO_FEED_POLL_SECONDS", json!(600), 1),
            row(GUILD, "TWO_RAID_JOIN_THRESHOLD", json!(3), 2),
        ],
    ));
    assert_eq!(report.hot.len(), 2, "{report:?}");
    assert_eq!(live.get(GUILD, "TWO_FEED_POLL_SECONDS"), Some(json!(600)));
    assert_eq!(live.get(GUILD, "TWO_RAID_JOIN_THRESHOLD"), Some(json!(3)));
}
