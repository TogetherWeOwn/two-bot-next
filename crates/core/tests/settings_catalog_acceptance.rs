//! Settings catalog validate-write and refresh-report acceptance.
//!
//! Pins the public `two_bot_core::settings` surface through behavior, not
//! census duplication: every catalog entry is driven through the classifiers,
//! the write guard, and the refresh partition by its declared class. Pure and
//! offline: no database, no network, and no process-environment reads. Key
//! names come only from the catalog constants, and loader defaults come from
//! empty-map calls. This file carries no key string literals, so the
//! container-env drift scanner keeps passing without reclassification.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use two_bot_core::settings::{
    assert_storable_key, classify_key, is_declared_env_only, is_env_only_key, is_storable_key,
    to_env_string, validate_write, IgnoreReason, SettingClass, SettingRow, SettingsCache,
    SettingsSnapshot, WriteAction, WriteRefusal, ENV_ONLY_KEY_PREFIXES, HOT_WIRED, SETTING_CLASSES,
};
use two_bot_core::{
    AutomodConfig, ClassifierConfig, FeatureGates, ModerationGates, OnboardingGates, ScorecardGates,
};

const GUILD: &str = "acceptance-guild";
const ACTOR: &str = "acceptance-actor";
const UNKNOWN_PROBE: &str = "UNLISTED_ACCEPTANCE_PROBE";

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

fn first_key(class: SettingClass) -> &'static str {
    SETTING_CLASSES
        .iter()
        .find(|(_, c)| *c == class)
        .map(|(key, _)| *key)
        .expect("catalog carries every class")
}

fn prefix_probe(suffix: &str) -> String {
    format!("{}{suffix}", ENV_ONLY_KEY_PREFIXES[0])
}

// ---------------------------------------------------------------------------
// (1) catalog partition: every entry classifies per its declared class,
//     unknown names fail closed, and the prefix refuses future gates.
// ---------------------------------------------------------------------------

#[test]
fn catalog_partition_matches_declared_classes() {
    let prefix: &str = ENV_ONLY_KEY_PREFIXES[0];
    assert!(!prefix.is_empty(), "prefix table must stay populated");
    for (key, class) in SETTING_CLASSES {
        assert_eq!(classify_key(key), Some(*class), "{key}");
        let guarded = *class == SettingClass::EnvOnly || key.starts_with(prefix);
        assert_eq!(is_env_only_key(key), guarded, "{key}");
        assert_eq!(is_storable_key(key), !guarded, "{key}");
        // Declared env-only is a policy answer about a classified key; a
        // storable key under the prefix would still report declared, so pin
        // the exact rule rather than assuming no overlap.
        assert_eq!(
            is_declared_env_only(key),
            *class == SettingClass::EnvOnly || key.starts_with(prefix),
            "{key}"
        );
    }
    // Unknown names fail closed: refused for storage, never declared.
    for probe in [UNKNOWN_PROBE, "ANOTHER_UNLISTED_SETTING"] {
        assert_eq!(classify_key(probe), None, "{probe}");
        assert!(is_env_only_key(probe), "{probe}");
        assert!(!is_storable_key(probe), "{probe}");
        assert!(!is_declared_env_only(probe), "{probe}");
        assert!(assert_storable_key(probe).is_err(), "{probe}");
    }
    // A gate added under the prefix tomorrow is refused before classification.
    for probe in [
        prefix_probe("FUTURE_GATE_PROBE"),
        prefix_probe("ALLOW_ANYTHING_PROBE"),
    ] {
        assert!(is_env_only_key(&probe), "{probe}");
        assert!(is_declared_env_only(&probe), "{probe}");
        assert!(!is_storable_key(&probe), "{probe}");
        assert!(assert_storable_key(&probe).is_err(), "{probe}");
    }
    // Every catalog key already under the prefix is env-only: dashboard
    // writes to that namespace are refused by construction.
    for (key, class) in SETTING_CLASSES
        .iter()
        .filter(|(key, _)| key.starts_with(prefix))
    {
        assert_eq!(*class, SettingClass::EnvOnly, "{key}");
        assert!(!is_storable_key(key), "{key}");
    }
}

// ---------------------------------------------------------------------------
// (2) assert_storable_key: storable classes pass, env-only and unknown refuse.
// ---------------------------------------------------------------------------

#[test]
fn assert_storable_key_refuses_env_only_and_unknown() {
    for (key, class) in SETTING_CLASSES {
        if *class == SettingClass::EnvOnly {
            let err = assert_storable_key(key).expect_err("env-only must refuse");
            assert_eq!(err.key.as_str(), *key);
        } else {
            assert!(assert_storable_key(key).is_ok(), "{key}");
        }
    }
    assert!(assert_storable_key(UNKNOWN_PROBE).is_err());
    assert!(assert_storable_key(&prefix_probe("PROBE_GATE")).is_err());
}

// ---------------------------------------------------------------------------
// (3) validate_write: per-key class decision plus the attribution and
//     encoding bounds, with ValidatedWrite / WriteRefusal / WriteAction
//     outcomes.
// ---------------------------------------------------------------------------

#[test]
fn validate_write_decides_per_key_class() {
    let prefix: &str = ENV_ONLY_KEY_PREFIXES[0];
    for (key, class) in SETTING_CLASSES {
        let result = validate_write(GUILD, key, Some(json!(1)), ACTOR);
        if *class == SettingClass::EnvOnly || key.starts_with(prefix) {
            assert!(
                matches!(result, Err(WriteRefusal::EnvOnly(_))),
                "{key} must refuse before any SQL"
            );
        } else {
            let write = result.unwrap_or_else(|_| panic!("storable key refused: {key}"));
            assert_eq!(write.class, *class, "{key}");
            assert_eq!(write.action, WriteAction::Upsert(json!(1)), "{key}");
            assert_eq!(write.guild_id, GUILD, "{key}");
            assert_eq!(write.key.as_str(), *key);
            assert_eq!(write.actor, ACTOR, "{key}");
        }
    }
}

#[test]
fn validate_write_outcome_matrix() {
    let hot = first_key(SettingClass::Hot);
    let cold = first_key(SettingClass::Cold);
    let env_only = first_key(SettingClass::EnvOnly);

    // Hot and cold validate; delete validates too (the documented undo path).
    let write = validate_write(GUILD, hot, Some(json!(3)), ACTOR).expect("hot validates");
    assert_eq!(write.class, SettingClass::Hot);
    assert_eq!(write.action, WriteAction::Upsert(json!(3)));
    let write = validate_write(GUILD, cold, Some(json!(600)), ACTOR).expect("cold validates");
    assert_eq!(write.class, SettingClass::Cold);
    assert_eq!(
        validate_write(GUILD, hot, None, ACTOR)
            .expect("delete validates")
            .action,
        WriteAction::Delete
    );

    // Refusals name the key; prefix-unlisted names refuse as env-only, plain
    // unlisted names refuse as unknown.
    match validate_write(GUILD, env_only, Some(json!(1)), ACTOR) {
        Err(WriteRefusal::EnvOnly(err)) => assert_eq!(err.key.as_str(), env_only),
        other => panic!("expected env-only refusal, got {other:?}"),
    }
    match validate_write(GUILD, &prefix_probe("PROBE_GATE"), Some(json!(1)), ACTOR) {
        Err(WriteRefusal::EnvOnly(_)) => {}
        other => panic!("expected prefix refusal, got {other:?}"),
    }
    match validate_write(GUILD, UNKNOWN_PROBE, Some(json!(1)), ACTOR) {
        Err(WriteRefusal::Unknown(name)) => assert_eq!(name, UNKNOWN_PROBE),
        other => panic!("expected unknown refusal, got {other:?}"),
    }

    // The key guard runs before attribution: a forbidden key with no actor is
    // still a key refusal, while a storable key with no actor is unattributed.
    assert!(matches!(
        validate_write(GUILD, env_only, Some(json!(1)), ""),
        Err(WriteRefusal::EnvOnly(_))
    ));
    assert!(matches!(
        validate_write(GUILD, UNKNOWN_PROBE, Some(json!(1)), ""),
        Err(WriteRefusal::Unknown(_))
    ));
    assert_eq!(
        validate_write(GUILD, hot, Some(json!(1)), ""),
        Err(WriteRefusal::MissingActor)
    );

    // Decoded NUL is refused in values, arrays, and object entries; a literal
    // backslash escape is valid text, not a NUL.
    assert_eq!(
        validate_write(GUILD, hot, Some(json!("synthetic\0value")), ACTOR),
        Err(WriteRefusal::NullCharacter)
    );
    assert_eq!(
        validate_write(GUILD, hot, Some(json!({"note": ["a\0b"]})), ACTOR),
        Err(WriteRefusal::NullCharacter)
    );
    assert!(
        validate_write(GUILD, hot, Some(json!("synthetic\\u0000value")), ACTOR).is_ok(),
        "escaped text is not a NUL byte"
    );
}

// ---------------------------------------------------------------------------
// (4) RefreshReport: hot-wired changes apply live, everything else storable
//     reports cold, env-only and unknown rows are ignored with a reason.
// ---------------------------------------------------------------------------

#[test]
fn hot_wired_is_a_writable_hot_subset() {
    assert!(!HOT_WIRED.is_empty(), "wired set must stay populated");
    for key in HOT_WIRED {
        assert_eq!(classify_key(key), Some(SettingClass::Hot), "{key}");
        assert!(is_storable_key(key), "{key}");
        assert!(assert_storable_key(key).is_ok(), "{key}");
    }
}

#[test]
fn refresh_partitions_hot_cold_and_ignored() {
    let wired: &str = HOT_WIRED[0];
    let unwired_hot: &str = SETTING_CLASSES
        .iter()
        .find_map(|(key, class)| {
            (*class == SettingClass::Hot && !HOT_WIRED.contains(key)).then_some(*key)
        })
        .expect("catalog carries hot keys outside the wired set");
    let cold = first_key(SettingClass::Cold);
    let env_only = first_key(SettingClass::EnvOnly);
    let prefixed = prefix_probe("REFRESH_PROBE");

    let mut cache = SettingsCache::load(&snapshot(0, vec![]));
    let loaded = snapshot(
        1,
        vec![
            row(GUILD, wired, json!(7), 1),
            row(GUILD, unwired_hot, json!(true), 1),
            row(GUILD, cold, json!("synthetic"), 1),
            row(GUILD, env_only, json!("synthetic"), 1),
            row(GUILD, UNKNOWN_PROBE, json!("synthetic"), 1),
            row(GUILD, &prefixed, json!("synthetic"), 1),
        ],
    );
    assert!(cache.needs_refresh(1, 6));
    let report = cache.refresh(&loaded);
    assert!(report.changed);
    assert_eq!((report.from_revision, report.to_revision), (0, 1));
    assert_eq!((report.from_rows, report.to_rows), (0, 6));

    // Exactly the wired key reports hot, with env-string from/to.
    assert_eq!(report.hot.len(), 1);
    let change = &report.hot[0];
    assert_eq!(change.guild_id, GUILD);
    assert_eq!(change.key, wired);
    assert_eq!(change.old, None);
    assert_eq!(change.new.as_deref(), Some("7"));

    // Hot-but-unwired reports cold (stored, restart to apply), next to cold.
    let cold_keys: HashSet<&str> = report.cold.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(cold_keys, HashSet::from([unwired_hot, cold]));
    let unwired = report
        .cold
        .iter()
        .find(|c| c.key == unwired_hot)
        .expect("unwired change");
    assert_eq!(unwired.old, None);
    assert_eq!(unwired.new.as_deref(), Some("1"));

    // Ignored rows carry the reason: declared env-only (catalog or prefix)
    // versus never-classified unknown.
    assert_eq!(report.ignored.len(), 3);
    let reasons: Vec<(&str, IgnoreReason)> = report
        .ignored
        .iter()
        .map(|i| (i.key.as_str(), i.reason))
        .collect();
    assert!(reasons.contains(&(env_only, IgnoreReason::EnvOnly)));
    assert!(reasons.contains(&(prefixed.as_str(), IgnoreReason::EnvOnly)));
    assert!(reasons.contains(&(UNKNOWN_PROBE, IgnoreReason::Unknown)));

    // Ignored rows never read back through either read API; storable rows do.
    assert_eq!(cache.get(GUILD, wired), Some(&json!(7)));
    assert_eq!(cache.get(GUILD, cold), Some(&json!("synthetic")));
    assert_eq!(cache.get(GUILD, env_only), None);
    assert_eq!(cache.get(GUILD, UNKNOWN_PROBE), None);
    let rendered = cache.env_snapshot(Some(GUILD));
    assert_eq!(rendered.get(wired).map(String::as_str), Some("7"));
    assert_eq!(rendered.get(cold).map(String::as_str), Some("synthetic"));
    assert!(!rendered.contains_key(env_only));
    assert!(!rendered.contains_key(UNKNOWN_PROBE));
    assert!(cache.env_snapshot(None).is_empty());
    assert!(!cache.needs_refresh(1, 6), "second poll is a no-op");

    // Deleting the wired row reports a hot change back to unset.
    let narrowed = snapshot(
        2,
        vec![
            row(GUILD, unwired_hot, json!(true), 1),
            row(GUILD, cold, json!("synthetic"), 1),
            row(GUILD, env_only, json!("synthetic"), 1),
            row(GUILD, UNKNOWN_PROBE, json!("synthetic"), 1),
            row(GUILD, &prefixed, json!("synthetic"), 1),
        ],
    );
    assert!(cache.needs_refresh(2, 5));
    let report = cache.refresh(&narrowed);
    assert!(report.changed);
    assert_eq!(report.hot.len(), 1);
    assert_eq!(report.hot[0].key, wired);
    assert_eq!(report.hot[0].old.as_deref(), Some("7"));
    assert_eq!(report.hot[0].new, None);
    assert_eq!(cache.get(GUILD, wired), None);
}

// ---------------------------------------------------------------------------
// (5) to_env_string: typed values render the way the environment carried
//     them, and empty-map loader defaults render without live IDs.
// ---------------------------------------------------------------------------

#[test]
fn env_rendering_round_trips_typed_values() {
    assert_eq!(to_env_string(&json!(true)), Some("1".to_owned()));
    assert_eq!(to_env_string(&json!(false)), Some("0".to_owned()));
    assert_eq!(to_env_string(&json!(42)), Some("42".to_owned()));
    assert_eq!(to_env_string(&json!(3.0)), Some("3".to_owned()));
    assert_eq!(to_env_string(&json!(3.5)), Some("3.5".to_owned()));
    assert_eq!(
        to_env_string(&json!("synthetic")),
        Some("synthetic".to_owned())
    );
    assert_eq!(to_env_string(&json!(["a", "b"])), Some("a,b".to_owned()));
    assert_eq!(to_env_string(&json!([])), Some(String::new()));
    assert_eq!(to_env_string(&Value::Null), None);
    let object = json!({"violations": 1});
    assert_eq!(to_env_string(&object), Some(object.to_string()));
    assert_eq!(to_env_string(&json!([3.0, 42])).as_deref(), Some("3,42"));
}

#[test]
fn empty_map_loader_defaults_render_without_live_ids() {
    let empty: HashMap<String, String> = HashMap::new();
    let features = FeatureGates::from_map(&empty).expect("empty map parses");
    assert!(!features.automations);
    assert!(!features.announcements);
    assert!(!features.text_commands);
    assert_eq!(features.feed_poll_seconds, 300);

    let automod = AutomodConfig::from_map(&empty).expect("empty map parses");
    assert!(!automod.enabled);
    assert!(automod.dry_run);
    assert_eq!(automod.policy.repeated_message_count, 3);
    assert_eq!(automod.policy.repeated_message_window_seconds, 30);
    assert_eq!(automod.policy.mention_limit, 5);
    assert!(automod.policy.bad_words.is_empty());
    assert!(automod.policy.bypass_role_ids.is_empty());
    assert!(automod.policy.exempt_channel_ids.is_empty());

    let scorecard = ScorecardGates::from_map(&empty).expect("empty map parses");
    assert!(!scorecard.enabled);
    assert!(scorecard.recommendations_enabled);
    assert_eq!(scorecard.correction_cycles, 0);

    let classifier = ClassifierConfig::from_map(&empty);
    assert_eq!(classifier.version, "community-v1");
    assert!(classifier.automation_actor_ids.is_empty());
    assert!(classifier.raid_actor_ids.is_empty());
    assert!(classifier.staging_guild_ids.is_empty());
    assert!(classifier.staging_actor_ids.is_empty());
    assert!(classifier.test_actor_ids.is_empty());

    // Disabled gates carry no identity: nothing to leak into a rendering.
    let moderation = ModerationGates::from_map(&empty).expect("empty map parses");
    assert!(!moderation.enabled);
    assert!(moderation.owen_user_id.is_empty());
    assert!(moderation.protected_role_ids.is_empty());

    let onboarding = OnboardingGates::from_map(&empty).expect("empty map parses");
    assert_eq!(onboarding.mode.as_str(), "legacy");
    assert!(!onboarding.dry_run);

    // Every default renders, and no rendering embeds a live ID.
    let snowflake = regex::Regex::new(r"\b[0-9]{17,20}\b").expect("static pattern");
    let rendered = [
        to_env_string(&json!(features.feed_poll_seconds)),
        to_env_string(&json!(features.automations)),
        to_env_string(&json!(automod.policy.repeated_message_count)),
        to_env_string(&json!(automod.policy.mention_limit)),
        to_env_string(&json!(scorecard.correction_cycles)),
        to_env_string(&json!(classifier.version)),
        to_env_string(&json!(moderation.enabled)),
        to_env_string(&json!(moderation.owen_user_id)),
        to_env_string(&json!(onboarding.dry_run)),
        to_env_string(&json!(onboarding.mode.as_str())),
    ];
    for value in rendered.into_iter().flatten() {
        assert!(!snowflake.is_match(&value), "{value}");
    }
    assert_eq!(
        to_env_string(&json!(features.feed_poll_seconds)).as_deref(),
        Some("300")
    );
}
