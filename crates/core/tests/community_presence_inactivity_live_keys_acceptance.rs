//! Protected acceptance: community, presence and inactivity keys go live.
//!
//! PROTECTED FILE: the paired Builder reclassifies the five keys, wires
//! their consumers to the live snapshot, and adds per-key write validation
//! without editing this file. Return necessary corrections to the Test
//! Author; never edit.
//!
//! Contract under test (all through today's public
//! `two_bot_core::settings` API plus the existing `ScorecardGates` /
//! `parse_inactivity_days` loaders — pure in-memory via `live_channel`, no
//! database, no network, no process-environment reads):
//!
//! - The five (`TWO_COMMUNITY_SCORECARD`, `TWO_COMMUNITY_RECOMMENDATIONS`,
//!   `TWO_COMMUNITY_CORRECTION_CYCLES`, `TWO_PRESENCE_PROBE`,
//!   `TWO_INACTIVITY_DAYS`) classify `Hot`, join `HOT_WIRED`, report hot
//!   (never cold) with env-string from/to on a live poll, and serve live
//!   per guild with no cross-guild leak.
//! - Stored rows override boot env store-first (the TOG-19027 pattern):
//!   layering the live `env_snapshot` over boot vars flips the effective
//!   scorecard gates, the presence gate (both directions), and the
//!   inactivity threshold; deleting a row reports a hot change back to
//!   boot (`new` = `None`) and the layered map carries boot again.
//! - Invalid writes are refused before any SQL (zero mutation): the three
//!   boolean gates take only boolean/0/1 with the existing typed
//!   `InvalidGateValue` code; correction-cycles and inactivity-days
//!   refusals name the key in both the typed code and the next-action
//!   message.
//! - B5 exclusion keys (reach/identity: moderation master, protected
//!   IDs, Owen ID, `TWO_INTERNAL_*`, assistant/synthetic IDs, plus the
//!   EnvOnly onboarding keys with no promotion path) stay `EnvOnly`,
//!   refuse writes, and are ignored (never served) on refresh; B7 honest
//!   gaps (warn/strike escalation, mute-role config) stay uninvented —
//!   no catalogue key claims those semantics.
//! - Fallback premises stay safe: absent rows read back as unset, the
//!   catalogue loaders refuse malformed input without panicking, only
//!   exactly `"1"` enables the scorecard gate (fail-closed), and the
//!   presence/recommendations gates stay on unless exactly `"0"`.

use std::collections::HashMap;

use serde_json::{json, Value};
use two_bot_core::settings::{
    classify_key, is_storable_key, live_channel, to_env_string, validate_write, KeyChange,
    RefreshReport, SettingClass, SettingRow, SettingsSnapshot, WriteAction, WriteRefusal,
    ENV_ONLY_KEY_PREFIXES, HOT_WIRED, POLL_SECONDS, SETTING_CLASSES,
};
use two_bot_core::{parse_inactivity_days, ScorecardGates};

const SCORECARD: &str = "TWO_COMMUNITY_SCORECARD";
const RECOMMENDATIONS: &str = "TWO_COMMUNITY_RECOMMENDATIONS";
const CORRECTION_CYCLES: &str = "TWO_COMMUNITY_CORRECTION_CYCLES";
const PRESENCE: &str = "TWO_PRESENCE_PROBE";
const INACTIVITY: &str = "TWO_INACTIVITY_DAYS";
const FIVE: [&str; 5] = [
    SCORECARD,
    RECOMMENDATIONS,
    CORRECTION_CYCLES,
    PRESENCE,
    INACTIVITY,
];

/// The three boolean gates: stored values must render to `0`/`1` only.
const BOOLEAN_GATES: [&str; 3] = [SCORECARD, RECOMMENDATIONS, PRESENCE];

const GUILD: &str = "acceptance-guild";
const OTHER_GUILD: &str = "other-acceptance-guild";
const ACTOR: &str = "acceptance-actor";

/// B5 PUT-allowlist exclusions: reach/identity keys that stay env-only,
/// plus the EnvOnly onboarding keys (documented exclusion, no promotion
/// path to Hot or Cold).
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
    "TWO_ONBOARDING_MODE",
    "TWO_ONBOARDING_ROTA_MEASUREMENT",
    "TWO_ONBOARDING_ROTA_NOTICE",
    "TWO_ONBOARDING_ROTA_PSEUDONYM_KEY",
    "TWO_ONBOARDING_ROTA_PRIMARY_ACTOR_ID",
    "TWO_ONBOARDING_ROTA_READER_IDS",
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

/// Presence gate as the community-jobs consumer reads it today
/// (`community_jobs.rs`): on unless explicitly `0`.
fn presence_enabled(vars: &HashMap<String, String>) -> bool {
    vars.get(PRESENCE).is_none_or(|v| v != "0")
}

fn inactivity_days(vars: &HashMap<String, String>) -> Result<u64, String> {
    parse_inactivity_days(vars.get(INACTIVITY).map(String::as_str)).map_err(|err| err.to_string())
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
    [
        (SCORECARD, "0"),
        (RECOMMENDATIONS, "0"),
        (CORRECTION_CYCLES, "0"),
        (PRESENCE, "0"),
        (INACTIVITY, "14"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

// ---------------------------------------------------------------------------
// (1) classification: the five are Hot and hot-wired.
// ---------------------------------------------------------------------------

#[test]
fn five_keys_are_classified_hot_and_hot_wired() {
    for key in FIVE {
        assert_eq!(classify_key(key), Some(SettingClass::Hot), "{key}");
        assert!(HOT_WIRED.contains(&key), "{key}");
        assert!(is_storable_key(key), "{key}");
    }
}

// ---------------------------------------------------------------------------
// (2) live poll: five rows report hot with env renderings and serve live.
// ---------------------------------------------------------------------------

#[test]
fn five_rows_report_hot_with_env_renderings_on_live_poll() {
    let (mut writer, live) = live_channel();
    let report = writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, SCORECARD, json!(true), 1),
            row(GUILD, RECOMMENDATIONS, json!(true), 2),
            row(GUILD, CORRECTION_CYCLES, json!(2), 3),
            row(GUILD, PRESENCE, json!(true), 4),
            row(GUILD, INACTIVITY, json!(30), 5),
        ],
    ));
    assert!(report.changed);
    assert_eq!(report.hot.len(), 5, "{report:?}");
    for key in FIVE {
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
    for (key, expected) in [
        (SCORECARD, "1"),
        (RECOMMENDATIONS, "1"),
        (CORRECTION_CYCLES, "2"),
        (PRESENCE, "1"),
        (INACTIVITY, "30"),
    ] {
        let change = hot_change(&report, key);
        assert_eq!(change.guild_id, GUILD, "{key}");
        assert_eq!(change.old, None, "{key}");
        assert_eq!(change.new.as_deref(), Some(expected), "{key}");
    }
    // Live serve per guild, with no cross-guild leak.
    assert_eq!(live.get(GUILD, SCORECARD), Some(json!(true)));
    assert_eq!(live.get(GUILD, RECOMMENDATIONS), Some(json!(true)));
    assert_eq!(live.get(GUILD, CORRECTION_CYCLES), Some(json!(2)));
    assert_eq!(live.get(GUILD, PRESENCE), Some(json!(true)));
    assert_eq!(live.get(GUILD, INACTIVITY), Some(json!(30)));
    for key in FIVE {
        assert_eq!(live.get(OTHER_GUILD, key), None, "{key}");
    }
    let rendered = live.env_snapshot(Some(GUILD));
    for (key, expected) in [
        (SCORECARD, "1"),
        (RECOMMENDATIONS, "1"),
        (CORRECTION_CYCLES, "2"),
        (PRESENCE, "1"),
        (INACTIVITY, "30"),
    ] {
        assert_eq!(
            rendered.get(key).map(String::as_str),
            Some(expected),
            "{key}"
        );
    }
    let other = live.env_snapshot(Some(OTHER_GUILD));
    for key in FIVE {
        assert!(!other.contains_key(key), "{key}");
    }
}

// ---------------------------------------------------------------------------
// (3) store-first: stored rows override boot env through the real loaders.
// ---------------------------------------------------------------------------

#[test]
fn five_stored_rows_override_boot_env_store_first() {
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, SCORECARD, json!(true), 1),
            row(GUILD, RECOMMENDATIONS, json!(true), 2),
            row(GUILD, CORRECTION_CYCLES, json!(2), 3),
            row(GUILD, PRESENCE, json!(true), 4),
            row(GUILD, INACTIVITY, json!(30), 5),
        ],
    ));
    let snap = live.env_snapshot(Some(GUILD));
    let boot = boot_vars();

    // Boot says off/off/0/off/14; the stored rows win per key.
    let vars = layer_key(&boot, &snap, SCORECARD);
    assert_eq!(vars.get(SCORECARD).map(String::as_str), Some("1"));
    assert!(
        ScorecardGates::from_map(&layer_key(&boot, &snap, SCORECARD))
            .expect("gates parse")
            .enabled
    );
    let vars = layer_key(&boot, &snap, RECOMMENDATIONS);
    assert!(
        ScorecardGates::from_map(&vars)
            .expect("gates parse")
            .recommendations_enabled
    );
    let vars = layer_key(&boot, &snap, CORRECTION_CYCLES);
    assert_eq!(
        ScorecardGates::from_map(&vars)
            .expect("gates parse")
            .correction_cycles,
        2
    );
    let vars = layer_key(&boot, &snap, PRESENCE);
    assert!(presence_enabled(&vars));
    let vars = layer_key(&boot, &snap, INACTIVITY);
    assert_eq!(inactivity_days(&vars).expect("days parse"), 30);

    // Boot alone carries the old behavior, proving the override direction.
    let gates = ScorecardGates::from_map(&boot).expect("boot parses");
    assert!(!gates.enabled);
    assert!(!gates.recommendations_enabled);
    assert_eq!(gates.correction_cycles, 0);
    assert!(!presence_enabled(&boot));
    assert_eq!(inactivity_days(&boot).expect("boot days parse"), 14);

    // The reverse direction: stored "0"/"14"/"0" disable over a boot "1"/"30"/"2".
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, SCORECARD, json!(false), 1),
            row(GUILD, RECOMMENDATIONS, json!(false), 2),
            row(GUILD, CORRECTION_CYCLES, json!(0), 3),
            row(GUILD, PRESENCE, json!(false), 4),
            row(GUILD, INACTIVITY, json!(14), 5),
        ],
    ));
    let snap = live.env_snapshot(Some(GUILD));
    let boot_on: HashMap<String, String> = [
        (SCORECARD, "1"),
        (RECOMMENDATIONS, "1"),
        (CORRECTION_CYCLES, "2"),
        (PRESENCE, "1"),
        (INACTIVITY, "30"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    let gates =
        ScorecardGates::from_map(&layer_key(&boot_on, &snap, SCORECARD)).expect("gates parse");
    assert!(!gates.enabled);
    let gates = ScorecardGates::from_map(&layer_key(&boot_on, &snap, RECOMMENDATIONS))
        .expect("gates parse");
    assert!(!gates.recommendations_enabled);
    let gates = ScorecardGates::from_map(&layer_key(&boot_on, &snap, CORRECTION_CYCLES))
        .expect("gates parse");
    assert_eq!(gates.correction_cycles, 0);
    assert!(!presence_enabled(&layer_key(&boot_on, &snap, PRESENCE)));
    assert_eq!(
        inactivity_days(&layer_key(&boot_on, &snap, INACTIVITY)).expect("days parse"),
        14
    );
}

// ---------------------------------------------------------------------------
// (4) delete: rows report hot changes back to boot and fall back.
// ---------------------------------------------------------------------------

#[test]
fn five_deletes_report_hot_changes_back_to_boot() {
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, SCORECARD, json!(true), 1),
            row(GUILD, RECOMMENDATIONS, json!(true), 2),
            row(GUILD, CORRECTION_CYCLES, json!(2), 3),
            row(GUILD, PRESENCE, json!(true), 4),
            row(GUILD, INACTIVITY, json!(30), 5),
        ],
    ));
    let report = writer.publish(&snapshot(2, vec![]));
    assert!(report.changed);
    assert_eq!(report.hot.len(), 5, "{report:?}");
    for (key, old) in [
        (SCORECARD, "1"),
        (RECOMMENDATIONS, "1"),
        (CORRECTION_CYCLES, "2"),
        (PRESENCE, "1"),
        (INACTIVITY, "30"),
    ] {
        let change = hot_change(&report, key);
        assert_eq!(change.old.as_deref(), Some(old), "{key}");
        assert_eq!(change.new, None, "{key}");
        assert_eq!(live.get(GUILD, key), None, "{key}");
    }
    // Absent rows fall back: the layered map carries boot again.
    let snap = live.env_snapshot(Some(GUILD));
    assert!(snap.keys().all(|key| !FIVE.contains(&key.as_str())));
    let boot = boot_vars();
    let vars = layer_key(&boot, &snap, SCORECARD);
    assert_eq!(vars.get(SCORECARD).map(String::as_str), Some("0"));
    assert!(
        !ScorecardGates::from_map(&vars)
            .expect("gates parse")
            .enabled
    );
    assert!(!presence_enabled(&layer_key(&boot, &snap, PRESENCE)));
    assert_eq!(
        inactivity_days(&layer_key(&boot, &snap, INACTIVITY)).expect("days parse"),
        14
    );
}

// ---------------------------------------------------------------------------
// (5) writes: invalid values refused with a typed code before any SQL.
// ---------------------------------------------------------------------------

#[test]
fn boolean_gates_invalid_writes_refused_with_typed_gate_code() {
    for key in BOOLEAN_GATES {
        for value in [json!("on"), json!("true"), json!(""), json!(2), json!(2.5)] {
            assert_eq!(
                validate_write(GUILD, key, Some(value.clone()), ACTOR),
                Err(WriteRefusal::InvalidGateValue(key.to_owned())),
                "{key} {value}"
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
            let write = validate_write(GUILD, key, Some(value.clone()), ACTOR)
                .unwrap_or_else(|err| panic!("valid gate refused {key} {value}: {err:?}"));
            assert_eq!(write.class, SettingClass::Hot, "{key} {value}");
            assert_eq!(write.action, WriteAction::Upsert(value), "{key}");
        }
        assert_eq!(
            validate_write(GUILD, key, None, ACTOR)
                .expect("delete validates")
                .action,
            WriteAction::Delete,
            "{key}"
        );
        assert_eq!(
            validate_write(GUILD, key, Some(json!(true)), ""),
            Err(WriteRefusal::MissingActor),
            "{key}"
        );
    }
}

#[test]
fn correction_cycles_invalid_writes_refused_naming_key() {
    for value in [
        json!("many"),
        json!("-1"),
        json!("1.5"),
        json!({"cycles": 2}),
    ] {
        let refusal = format!("invalid correction-cycles write refused: {value}");
        let err = validate_write(GUILD, CORRECTION_CYCLES, Some(value.clone()), ACTOR)
            .expect_err(&refusal);
        assert!(
            format!("{err:?}").contains(CORRECTION_CYCLES),
            "typed code names the key: {err:?}"
        );
        assert!(
            err.to_string().contains(CORRECTION_CYCLES),
            "next action names the key: {err}"
        );
    }
    for value in [json!(2), json!("2"), json!(0)] {
        let write = validate_write(GUILD, CORRECTION_CYCLES, Some(value.clone()), ACTOR)
            .unwrap_or_else(|err| panic!("valid cycles refused {value}: {err:?}"));
        assert_eq!(write.class, SettingClass::Hot, "{value}");
        assert_eq!(write.action, WriteAction::Upsert(value));
    }
    assert_eq!(
        validate_write(GUILD, CORRECTION_CYCLES, None, ACTOR)
            .expect("delete validates")
            .action,
        WriteAction::Delete
    );
    assert_eq!(
        validate_write(GUILD, CORRECTION_CYCLES, Some(json!(2)), ""),
        Err(WriteRefusal::MissingActor)
    );
}

#[test]
fn inactivity_days_invalid_writes_refused_naming_key() {
    for value in [json!("many"), json!(-5), json!("1.5"), json!({"days": 30})] {
        let refusal = format!("invalid inactivity-days write refused: {value}");
        let err =
            validate_write(GUILD, INACTIVITY, Some(value.clone()), ACTOR).expect_err(&refusal);
        assert!(
            format!("{err:?}").contains(INACTIVITY),
            "typed code names the key: {err:?}"
        );
        assert!(
            err.to_string().contains(INACTIVITY),
            "next action names the key: {err}"
        );
    }
    for value in [json!(30), json!("30"), json!(14)] {
        let write = validate_write(GUILD, INACTIVITY, Some(value.clone()), ACTOR)
            .unwrap_or_else(|err| panic!("valid days refused {value}: {err:?}"));
        assert_eq!(write.class, SettingClass::Hot, "{value}");
        assert_eq!(write.action, WriteAction::Upsert(value));
    }
    assert_eq!(
        validate_write(GUILD, INACTIVITY, None, ACTOR)
            .expect("delete validates")
            .action,
        WriteAction::Delete
    );
    assert_eq!(
        validate_write(GUILD, INACTIVITY, Some(json!(30)), ""),
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
    for key in FIVE {
        assert_eq!(live.get(GUILD, key), None, "{key}");
    }
    assert!(live.env_snapshot(Some(GUILD)).is_empty());
    // Scorecard premise: only exactly "1" enables; anything else is safely
    // off (fail-closed) without panicking.
    for (raw, expected) in [
        ("1", true),
        ("0", false),
        ("on", false),
        ("true", false),
        ("", false),
        ("2", false),
    ] {
        let vars: HashMap<String, String> = [(SCORECARD.to_owned(), raw.to_owned())]
            .into_iter()
            .collect();
        assert_eq!(
            ScorecardGates::from_map(&vars)
                .expect("gates parse")
                .enabled,
            expected,
            "{raw:?}"
        );
    }
    // Recommendations/presence premise: on unless exactly "0".
    for (key, raw, expected) in [
        (RECOMMENDATIONS, "1", true),
        (RECOMMENDATIONS, "0", false),
        (RECOMMENDATIONS, "on", true),
        (PRESENCE, "1", true),
        (PRESENCE, "0", false),
        (PRESENCE, "on", true),
    ] {
        let vars: HashMap<String, String> =
            [(key.to_owned(), raw.to_owned())].into_iter().collect();
        if key == RECOMMENDATIONS {
            assert_eq!(
                ScorecardGates::from_map(&vars)
                    .expect("gates parse")
                    .recommendations_enabled,
                expected,
                "{key} {raw:?}"
            );
        } else {
            assert_eq!(presence_enabled(&vars), expected, "{key} {raw:?}");
        }
    }
    // Correction-cycles premise: default 0, parses u32, refuses garbage
    // without panicking (the live reader's fallback trigger).
    assert_eq!(
        ScorecardGates::from_map(&HashMap::new())
            .expect("defaults parse")
            .correction_cycles,
        0
    );
    for raw in ["many", "-1", "1.5"] {
        let vars: HashMap<String, String> = [(CORRECTION_CYCLES.to_owned(), raw.to_owned())]
            .into_iter()
            .collect();
        assert!(ScorecardGates::from_map(&vars).is_err(), "{raw}");
    }
    // Inactivity premise: default 14, parses u64, refuses garbage.
    assert_eq!(parse_inactivity_days(None).expect("default parses"), 14);
    assert_eq!(parse_inactivity_days(Some("30")).expect("days parse"), 30);
    for raw in ["many", "-5", "1.5"] {
        assert!(parse_inactivity_days(Some(raw)).is_err(), "{raw}");
    }
    // Stored numerics render as plain integer text, the shape the live
    // readers parse with boot fallback.
    assert_eq!(to_env_string(&json!(2)).as_deref(), Some("2"));
    assert_eq!(to_env_string(&json!(30)).as_deref(), Some("30"));
    assert_eq!(to_env_string(&json!(true)).as_deref(), Some("1"));
    assert_eq!(to_env_string(&json!("many")).as_deref(), Some("many"));
}

// ---------------------------------------------------------------------------
// (8) control: already-wired keys stay hot-wired within one poll.
// ---------------------------------------------------------------------------

#[test]
fn control_feed_interval_and_raid_threshold_stay_hot_wired() {
    assert_eq!(POLL_SECONDS, 15, "dashboard writes apply within one poll");
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
