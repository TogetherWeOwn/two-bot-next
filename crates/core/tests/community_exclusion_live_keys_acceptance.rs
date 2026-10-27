//! Protected acceptance: stored actor and staging exclusions reach new facts.
//!
//! PROTECTED FILE: the paired Builder wires the five exclusion consumers to
//! the live snapshot, extends `HOT_WIRED`, updates the generated reference
//! truth plus setting-change diagnostics, and documents verification —
//! without editing this file. Return necessary corrections to the Test
//! Author; never edit.
//!
//! Contract under test (all through today's public `two_bot_core` API —
//! `settings::live_channel`, `ClassifierConfig::from_map`, `classify`,
//! `FunnelHandlers::on_message`, `FactRow`/`build_scorecard` — plus reads of
//! the committed reference docs; pure in-memory, no database, no network,
//! no process-environment reads):
//!
//! - The five (`TWO_COMMUNITY_AUTOMATION_ACTOR_IDS`,
//!   `TWO_COMMUNITY_RAID_ACTOR_IDS`, `TWO_COMMUNITY_STAGING_GUILD_IDS`,
//!   `TWO_COMMUNITY_STAGING_ACTOR_IDS`, `TWO_COMMUNITY_TEST_ACTOR_IDS`)
//!   classify `Hot`, join `HOT_WIRED` (39 entries), report hot (never cold)
//!   with env-string from/to on a live poll, and serve live per guild with
//!   no cross-guild leak.
//! - Stored rows override boot env store-first (the TOG-19027 pattern): one
//!   `env_snapshot` call layers all five over boot vars and flips the
//!   effective `ClassifierConfig` sets plus `classify` verdicts for each
//!   exclusion; deleting a row reports a hot change back to boot (`new` =
//!   `None`) and the layered map carries boot again.
//! - Explicit empty (`""` or `[]`) retains empty semantics: it overlays as
//!   the empty set and beats a non-empty boot. JSON null is omitted and
//!   falls back to boot (the documented unset shape, not empty).
//! - Precedence is preserved: bot → webhook → staff automation → raid →
//!   staging → test → eligible human, with configured sets participating at
//!   their level and event flags (`is_staff_automation`, `is_raid`,
//!   `is_staging`, `is_test`) at the same levels.
//! - Hostile stored values (whitespace, empty entries, duplicates, JSON
//!   object/number/nested) never crash, never leak across guilds, and never
//!   falsely exclude: only a literal actor/guild match excludes.
//! - Zero-effect boundaries: historical `FactRow` classifications are never
//!   rewritten (old snapshots stay immutable generations); capture still
//!   records every message fact with its actor intact (so downstream
//!   privacy suppression still applies); funnel awards still gate only
//!   bots/webhooks/staff (raid/staging/test still produce funnel events,
//!   as today); human/welcome channel sets and the five
//!   scorecard/presence/inactivity gates stay live and unchanged.
//! - Refresh atomicity: one snapshot carrying all five publishes and serves
//!   all five together, never a mixed subset. A poll that does not publish
//!   preserves the last confirmed snapshot (same `Arc`, no clearing).
//! - Bound reads: one `env_snapshot` per refresh serves all five; this file
//!   runs fully offline (no `TWO_TEST_DATABASE_URL`, no `TestDatabase`, no
//!   `sqlx`, no `from_env`), proving no per-message database query.
//! - Reference truth: `docs/configuration.md` lists the five as live-applied
//!   (not unwired); `docs/community-jobs.md` documents this file's focused
//!   offline verification command (discoverable via `docs/README.md`);
//!   `docs/metrics.md` gains no per-actor labels.
//! - AX: stable key IDs through refresh; bounded write refusals that never
//!   echo submitted values; no snowflake/actor dumps in the reference docs.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use two_bot_core::settings::{
    classify_key, is_storable_key, live_channel, to_env_string, validate_write, KeyChange,
    RefreshReport, SettingClass, SettingRow, SettingsSnapshot, WriteAction, WriteRefusal,
    HOT_WIRED,
};
use two_bot_core::{
    build_scorecard, classify, ChannelClass, ClassifierConfig, ClassifyInput, FactRow, FactsSink,
    FunnelHandlers, MemStore, MemberJoinFact, MessageFact, MessageInput, NoopLeveling,
    RulesAcceptedFact, ScorecardInputs, StreamCoverage, VoiceEndedFact, VoiceStartedFact,
    COMMUNITY_FACT_TYPES,
};

const AUTOMATION: &str = "TWO_COMMUNITY_AUTOMATION_ACTOR_IDS";
const RAID: &str = "TWO_COMMUNITY_RAID_ACTOR_IDS";
const STAGING_GUILDS: &str = "TWO_COMMUNITY_STAGING_GUILD_IDS";
const STAGING_ACTORS: &str = "TWO_COMMUNITY_STAGING_ACTOR_IDS";
const TEST: &str = "TWO_COMMUNITY_TEST_ACTOR_IDS";
const FIVE: [&str; 5] = [AUTOMATION, RAID, STAGING_GUILDS, STAGING_ACTORS, TEST];

const GUILD: &str = "acceptance-guild";
const OTHER_GUILD: &str = "other-acceptance-guild";
const ACTOR: &str = "acceptance-actor";

// Synthetic exclusion members. Readable strings, never snowflakes: `id_set`
// accepts any non-empty trimmed entry, and readable fixtures cannot trip the
// reference-docs snowflake tripwire.
const AUTOMATION_ACTOR: &str = "automation-actor-1";
const RAID_ACTOR: &str = "raid-actor-1";
const STAGING_GUILD: &str = "staging-guild-1";
const STAGING_ACTOR: &str = "staging-actor-1";
const TEST_ACTOR: &str = "test-actor-1";
const HUMAN_ACTOR: &str = "human-1";

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

/// Bound-reads composition for all five: ONE snapshot map layers all five.
/// Production calls `live.env_snapshot` once per refresh and overlays the
/// five from that single map — never five database reads, never five polls.
fn layer_five(
    boot: &HashMap<String, String>,
    live: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut vars = boot.clone();
    for key in FIVE {
        if let Some(value) = live.get(key) {
            vars.insert(key.to_owned(), value.clone());
        }
    }
    vars
}

fn boot_vars() -> HashMap<String, String> {
    // Boot carries no exclusions: every fixture actor classifies human until
    // a stored row wins.
    HashMap::new()
}

fn boot_vars_with_exclusions() -> HashMap<String, String> {
    [
        (AUTOMATION, "boot-automation-1"),
        (RAID, "boot-raid-1"),
        (STAGING_GUILDS, "boot-staging-guild-1"),
        (STAGING_ACTORS, "boot-staging-actor-1"),
        (TEST, "boot-test-1"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

fn input_for(actor: &str, guild: &str) -> ClassifyInput {
    ClassifyInput {
        guild_id: guild.to_owned(),
        actor_id: actor.to_owned(),
        is_bot: false,
        webhook_id: None,
        is_staff_automation: false,
        is_raid: false,
        is_staging: false,
        is_test: false,
    }
}

fn verdict_for(config: &ClassifierConfig, actor: &str, guild: &str) -> String {
    classify(config, &input_for(actor, guild)).classification
}

fn repository_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_doc(relative: &str) -> String {
    std::fs::read_to_string(repository_root().join(relative))
        .unwrap_or_else(|err| panic!("read {relative}: {err}"))
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
    // 34 wired before this slice, plus these five.
    assert_eq!(HOT_WIRED.len(), 39, "HOT_WIRED must gain exactly the five");
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
            row(GUILD, AUTOMATION, json!([AUTOMATION_ACTOR]), 1),
            row(GUILD, RAID, json!([RAID_ACTOR]), 2),
            row(GUILD, STAGING_GUILDS, json!([STAGING_GUILD]), 3),
            row(GUILD, STAGING_ACTORS, json!([STAGING_ACTOR]), 4),
            row(GUILD, TEST, json!([TEST_ACTOR]), 5),
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
    for (key, expected) in [
        (AUTOMATION, AUTOMATION_ACTOR),
        (RAID, RAID_ACTOR),
        (STAGING_GUILDS, STAGING_GUILD),
        (STAGING_ACTORS, STAGING_ACTOR),
        (TEST, TEST_ACTOR),
    ] {
        let change = hot_change(&report, key);
        assert_eq!(change.guild_id, GUILD, "{key}");
        assert_eq!(change.old, None, "{key}");
        assert_eq!(change.new.as_deref(), Some(expected), "{key}");
    }
    assert_eq!(live.get(GUILD, AUTOMATION), Some(json!([AUTOMATION_ACTOR])));
    assert_eq!(live.get(GUILD, RAID), Some(json!([RAID_ACTOR])));
    assert_eq!(
        live.get(GUILD, STAGING_GUILDS),
        Some(json!([STAGING_GUILD]))
    );
    assert_eq!(
        live.get(GUILD, STAGING_ACTORS),
        Some(json!([STAGING_ACTOR]))
    );
    assert_eq!(live.get(GUILD, TEST), Some(json!([TEST_ACTOR])));
    for key in FIVE {
        assert_eq!(live.get(OTHER_GUILD, key), None, "{key}");
    }
    let rendered = live.env_snapshot(Some(GUILD));
    for (key, expected) in [
        (AUTOMATION, AUTOMATION_ACTOR),
        (RAID, RAID_ACTOR),
        (STAGING_GUILDS, STAGING_GUILD),
        (STAGING_ACTORS, STAGING_ACTOR),
        (TEST, TEST_ACTOR),
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
// (3) store-first: stored rows override boot env through the real classifier.
// ---------------------------------------------------------------------------

#[test]
fn five_stored_rows_override_boot_env_store_first() {
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, AUTOMATION, json!([AUTOMATION_ACTOR]), 1),
            row(GUILD, RAID, json!([RAID_ACTOR]), 2),
            row(GUILD, STAGING_GUILDS, json!([STAGING_GUILD]), 3),
            row(GUILD, STAGING_ACTORS, json!([STAGING_ACTOR]), 4),
            row(GUILD, TEST, json!([TEST_ACTOR]), 5),
        ],
    ));
    // ONE snapshot read layers all five (bound reads).
    let snap = live.env_snapshot(Some(GUILD));
    let boot = boot_vars();

    // Boot says no exclusions; the stored rows win per key through the real
    // loader plus the real precedence function.
    let layered = ClassifierConfig::from_map(&layer_five(&boot, &snap));
    assert_eq!(
        verdict_for(&layered, AUTOMATION_ACTOR, GUILD),
        "staff_automation"
    );
    assert_eq!(verdict_for(&layered, RAID_ACTOR, GUILD), "raid");
    assert_eq!(
        verdict_for(&layered, HUMAN_ACTOR, STAGING_GUILD),
        "staging",
        "staging guild excludes every actor in it"
    );
    assert_eq!(verdict_for(&layered, STAGING_ACTOR, GUILD), "staging");
    assert_eq!(verdict_for(&layered, TEST_ACTOR, GUILD), "test");
    assert_eq!(verdict_for(&layered, HUMAN_ACTOR, GUILD), "eligible_human");

    // Boot alone carries the old behavior, proving the override direction.
    let boot_config = ClassifierConfig::from_map(&boot);
    for actor in [
        AUTOMATION_ACTOR,
        RAID_ACTOR,
        STAGING_ACTOR,
        TEST_ACTOR,
        HUMAN_ACTOR,
    ] {
        assert_eq!(
            verdict_for(&boot_config, actor, GUILD),
            "eligible_human",
            "{actor}"
        );
    }

    // The reverse direction: stored rows replace boot IDs entirely (winning,
    // not merging). Boot's IDs classify human once the stored set wins.
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, AUTOMATION, json!([AUTOMATION_ACTOR]), 1),
            row(GUILD, RAID, json!([RAID_ACTOR]), 2),
            row(GUILD, STAGING_GUILDS, json!([STAGING_GUILD]), 3),
            row(GUILD, STAGING_ACTORS, json!([STAGING_ACTOR]), 4),
            row(GUILD, TEST, json!([TEST_ACTOR]), 5),
        ],
    ));
    let snap = live.env_snapshot(Some(GUILD));
    let boot = boot_vars_with_exclusions();
    let layered = ClassifierConfig::from_map(&layer_five(&boot, &snap));
    assert_eq!(
        verdict_for(&layered, AUTOMATION_ACTOR, GUILD),
        "staff_automation"
    );
    assert_eq!(
        verdict_for(&layered, "boot-automation-1", GUILD),
        "eligible_human",
        "stored set replaces boot, never merges"
    );
    assert_eq!(verdict_for(&layered, RAID_ACTOR, GUILD), "raid");
    assert_eq!(
        verdict_for(&layered, TEST_ACTOR, GUILD),
        "test",
        "stored test set replaces boot test set"
    );
    assert_eq!(
        verdict_for(&layered, "boot-test-1", GUILD),
        "eligible_human"
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
            row(GUILD, AUTOMATION, json!([AUTOMATION_ACTOR]), 1),
            row(GUILD, RAID, json!([RAID_ACTOR]), 2),
            row(GUILD, STAGING_GUILDS, json!([STAGING_GUILD]), 3),
            row(GUILD, STAGING_ACTORS, json!([STAGING_ACTOR]), 4),
            row(GUILD, TEST, json!([TEST_ACTOR]), 5),
        ],
    ));
    let report = writer.publish(&snapshot(2, vec![]));
    assert!(report.changed);
    assert_eq!(report.hot.len(), 5, "{report:?}");
    for (key, old) in [
        (AUTOMATION, AUTOMATION_ACTOR),
        (RAID, RAID_ACTOR),
        (STAGING_GUILDS, STAGING_GUILD),
        (STAGING_ACTORS, STAGING_ACTOR),
        (TEST, TEST_ACTOR),
    ] {
        let change = hot_change(&report, key);
        assert_eq!(change.old.as_deref(), Some(old), "{key}");
        assert_eq!(change.new, None, "{key}");
        assert_eq!(live.get(GUILD, key), None, "{key}");
    }
    // Absent rows fall back: the layered map carries boot again.
    let snap = live.env_snapshot(Some(GUILD));
    assert!(snap.keys().all(|key| !FIVE.contains(&key.as_str())));
    let boot = boot_vars_with_exclusions();
    let layered = ClassifierConfig::from_map(&layer_five(&boot, &snap));
    assert_eq!(
        verdict_for(&layered, "boot-automation-1", GUILD),
        "staff_automation"
    );
    assert_eq!(
        verdict_for(&layered, AUTOMATION_ACTOR, GUILD),
        "eligible_human",
        "deleted stored ID no longer excludes"
    );
    assert_eq!(verdict_for(&layered, "boot-test-1", GUILD), "test");
}

// ---------------------------------------------------------------------------
// (5) explicit empty retains empty; null falls back to boot.
// ---------------------------------------------------------------------------

#[test]
fn explicit_empty_stored_values_beat_boot_with_empty_sets() {
    for empty in [json!(""), json!([])] {
        let (mut writer, live) = live_channel();
        writer.publish(&snapshot(
            1,
            FIVE.iter()
                .enumerate()
                .map(|(i, key)| row(GUILD, key, empty.clone(), i as i64 + 1))
                .collect(),
        ));
        let snap = live.env_snapshot(Some(GUILD));
        // The snapshot carries the keys with empty renderings (present and
        // empty, not absent): that presence is what beats boot.
        for key in FIVE {
            assert_eq!(snap.get(key).map(String::as_str), Some(""), "{key} {empty}");
        }
        let boot = boot_vars_with_exclusions();
        let layered = ClassifierConfig::from_map(&layer_five(&boot, &snap));
        assert!(layered.automation_actor_ids.is_empty(), "{empty}");
        assert!(layered.raid_actor_ids.is_empty(), "{empty}");
        assert!(layered.staging_guild_ids.is_empty(), "{empty}");
        assert!(layered.staging_actor_ids.is_empty(), "{empty}");
        assert!(layered.test_actor_ids.is_empty(), "{empty}");
        assert_eq!(
            verdict_for(&layered, "boot-automation-1", GUILD),
            "eligible_human",
            "stored empty clears boot automation: {empty}"
        );
        assert_eq!(
            verdict_for(&layered, "boot-test-1", GUILD),
            "eligible_human",
            "stored empty clears boot test: {empty}"
        );
    }
}

#[test]
fn control_json_null_is_omitted_and_falls_back_to_boot() {
    // `to_env_string(Value::Null)` is `None`: a null row is omitted from the
    // snapshot and falls through to boot. Null is unset, never empty.
    assert_eq!(to_env_string(&Value::Null), None);
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        FIVE.iter()
            .enumerate()
            .map(|(i, key)| row(GUILD, key, Value::Null, i as i64 + 1))
            .collect(),
    ));
    let snap = live.env_snapshot(Some(GUILD));
    for key in FIVE {
        assert!(!snap.contains_key(key), "{key}");
    }
    let boot = boot_vars_with_exclusions();
    let layered = ClassifierConfig::from_map(&layer_five(&boot, &snap));
    assert_eq!(
        verdict_for(&layered, "boot-automation-1", GUILD),
        "staff_automation"
    );
    assert_eq!(verdict_for(&layered, "boot-test-1", GUILD), "test");
}

// ---------------------------------------------------------------------------
// (6) per-guild isolation.
// ---------------------------------------------------------------------------

#[test]
fn stored_exclusions_never_leak_across_guilds() {
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, AUTOMATION, json!([AUTOMATION_ACTOR]), 1),
            row(GUILD, RAID, json!([RAID_ACTOR]), 2),
            row(GUILD, STAGING_GUILDS, json!([STAGING_GUILD]), 3),
            row(GUILD, STAGING_ACTORS, json!([STAGING_ACTOR]), 4),
            row(GUILD, TEST, json!([TEST_ACTOR]), 5),
        ],
    ));
    let boot = boot_vars();
    let own = ClassifierConfig::from_map(&layer_five(&boot, &live.env_snapshot(Some(GUILD))));
    let other =
        ClassifierConfig::from_map(&layer_five(&boot, &live.env_snapshot(Some(OTHER_GUILD))));
    assert_eq!(
        verdict_for(&own, AUTOMATION_ACTOR, GUILD),
        "staff_automation"
    );
    assert_eq!(verdict_for(&own, TEST_ACTOR, GUILD), "test");
    for actor in [AUTOMATION_ACTOR, RAID_ACTOR, STAGING_ACTOR, TEST_ACTOR] {
        assert_eq!(
            verdict_for(&other, actor, OTHER_GUILD),
            "eligible_human",
            "{actor} must stay human in the unconfigured guild"
        );
    }
    // Staging guild scope is guild-scoped too: the staged guild excludes,
    // the neighbouring guild does not.
    assert_eq!(
        verdict_for(&own, HUMAN_ACTOR, STAGING_GUILD),
        "staging",
        "layered through the owning guild's snapshot"
    );
    let staged_elsewhere =
        ClassifierConfig::from_map(&layer_five(&boot, &live.env_snapshot(Some(STAGING_GUILD))));
    assert_eq!(
        verdict_for(&staged_elsewhere, HUMAN_ACTOR, STAGING_GUILD),
        "eligible_human",
        "a guild is staged only when its own snapshot names it"
    );

    // Distinct rows per guild stay distinct.
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, TEST, json!(["guild-a-test-1"]), 1),
            row(OTHER_GUILD, TEST, json!(["guild-b-test-1"]), 2),
        ],
    ));
    let snap_a = live.env_snapshot(Some(GUILD));
    let snap_b = live.env_snapshot(Some(OTHER_GUILD));
    assert_eq!(snap_a.get(TEST).map(String::as_str), Some("guild-a-test-1"));
    assert_eq!(snap_b.get(TEST).map(String::as_str), Some("guild-b-test-1"));
    let config_a = ClassifierConfig::from_map(&layer_key(&boot, &snap_a, TEST));
    let config_b = ClassifierConfig::from_map(&layer_key(&boot, &snap_b, TEST));
    assert_eq!(verdict_for(&config_a, "guild-a-test-1", GUILD), "test");
    assert_eq!(
        verdict_for(&config_a, "guild-b-test-1", GUILD),
        "eligible_human"
    );
    assert_eq!(
        verdict_for(&config_b, "guild-b-test-1", OTHER_GUILD),
        "test"
    );
    assert_eq!(
        verdict_for(&config_b, "guild-a-test-1", OTHER_GUILD),
        "eligible_human"
    );
}

// ---------------------------------------------------------------------------
// (7) precedence preserved (controls: direct loader, no live snapshot).
// ---------------------------------------------------------------------------

#[test]
fn control_configured_sets_participate_in_legacy_precedence() {
    let config = ClassifierConfig::from_map(
        &[
            (AUTOMATION, AUTOMATION_ACTOR),
            (RAID, RAID_ACTOR),
            (STAGING_GUILDS, STAGING_GUILD),
            (STAGING_ACTORS, STAGING_ACTOR),
            (TEST, TEST_ACTOR),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect(),
    );
    // A bot posting through a webhook is one bot bucket, not two.
    let both = ClassifyInput {
        webhook_id: Some("webhook-1".to_owned()),
        ..input_for(AUTOMATION_ACTOR, GUILD)
    };
    let mut both = both;
    both.is_bot = true;
    assert_eq!(classify(&config, &both).classification, "bot");
    // A bot in every exclusion set is still a bot.
    for actor in [AUTOMATION_ACTOR, RAID_ACTOR, STAGING_ACTOR, TEST_ACTOR] {
        let mut flagged = input_for(actor, GUILD);
        flagged.is_bot = true;
        assert_eq!(classify(&config, &flagged).classification, "bot", "{actor}");
    }
    // A webhook in every exclusion set is still a webhook.
    for actor in [AUTOMATION_ACTOR, RAID_ACTOR, STAGING_ACTOR, TEST_ACTOR] {
        let mut flagged = input_for(actor, GUILD);
        flagged.webhook_id = Some("webhook-1".to_owned());
        assert_eq!(
            classify(&config, &flagged).classification,
            "webhook",
            "{actor}"
        );
    }
    // Automation wins over raid/staging/test sets.
    let mut multi = input_for(HUMAN_ACTOR, GUILD);
    multi.actor_id = AUTOMATION_ACTOR.to_owned();
    assert_eq!(classify(&config, &multi).classification, "staff_automation");
    // Event flags participate at their own levels, above lower sets.
    let mut flagged = input_for(RAID_ACTOR, GUILD);
    flagged.is_staff_automation = true;
    assert_eq!(
        classify(&config, &flagged).classification,
        "staff_automation"
    );
    let mut flagged = input_for(STAGING_ACTOR, GUILD);
    flagged.is_raid = true;
    assert_eq!(classify(&config, &flagged).classification, "raid");
    let mut flagged = input_for(TEST_ACTOR, GUILD);
    flagged.is_staging = true;
    assert_eq!(classify(&config, &flagged).classification, "staging");
    let mut flagged = input_for(HUMAN_ACTOR, GUILD);
    flagged.is_test = true;
    assert_eq!(classify(&config, &flagged).classification, "test");
    // Set order below webhook: automation > raid > staging > test > human.
    assert_eq!(
        classify(&config, &input_for(AUTOMATION_ACTOR, GUILD)).classification,
        "staff_automation"
    );
    assert_eq!(
        classify(&config, &input_for(RAID_ACTOR, GUILD)).classification,
        "raid"
    );
    assert_eq!(
        classify(&config, &input_for(STAGING_ACTOR, GUILD)).classification,
        "staging"
    );
    assert_eq!(
        classify(&config, &input_for(TEST_ACTOR, GUILD)).classification,
        "test"
    );
    assert_eq!(
        classify(&config, &input_for(HUMAN_ACTOR, GUILD)).classification,
        "eligible_human"
    );
    // Staging guild scope excludes every actor in that guild, human or not.
    let staged = ClassifyInput {
        guild_id: STAGING_GUILD.to_owned(),
        ..input_for(HUMAN_ACTOR, STAGING_GUILD)
    };
    assert_eq!(classify(&config, &staged).classification, "staging");
    // An empty webhook id is not a webhook.
    let mut empty_hook = input_for(HUMAN_ACTOR, GUILD);
    empty_hook.webhook_id = Some(String::new());
    assert_eq!(
        classify(&config, &empty_hook).classification,
        "eligible_human"
    );
}

// ---------------------------------------------------------------------------
// (8) hostile stored values: never crash, never leak, never falsely exclude.
// ---------------------------------------------------------------------------

#[test]
fn hostile_stored_values_with_real_ids_still_exclude_after_trimming() {
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(
                GUILD,
                AUTOMATION,
                json!("  , automation-actor-1 ,,  automation-actor-1 "),
                1,
            ),
            row(GUILD, RAID, json!(["", "  ", RAID_ACTOR, RAID_ACTOR]), 2),
            row(GUILD, TEST, json!([TEST_ACTOR, ""]), 3),
        ],
    ));
    let snap = live.env_snapshot(Some(GUILD));
    let layered = ClassifierConfig::from_map(&layer_five(&boot_vars(), &snap));
    // Whitespace trimmed, empty entries dropped, duplicates deduped.
    assert_eq!(layered.automation_actor_ids.len(), 1);
    assert!(layered.automation_actor_ids.contains(AUTOMATION_ACTOR));
    assert_eq!(layered.raid_actor_ids.len(), 1);
    assert_eq!(
        verdict_for(&layered, AUTOMATION_ACTOR, GUILD),
        "staff_automation"
    );
    assert_eq!(verdict_for(&layered, RAID_ACTOR, GUILD), "raid");
    assert_eq!(verdict_for(&layered, TEST_ACTOR, GUILD), "test");
    assert_eq!(
        verdict_for(&layered, HUMAN_ACTOR, GUILD),
        "eligible_human",
        "empty entries never match"
    );
    // No leak into the neighbouring guild.
    let other = ClassifierConfig::from_map(&layer_five(
        &boot_vars(),
        &live.env_snapshot(Some(OTHER_GUILD)),
    ));
    assert_eq!(
        verdict_for(&other, AUTOMATION_ACTOR, OTHER_GUILD),
        "eligible_human"
    );
}

#[test]
fn control_pure_garbage_stored_values_never_crash_nor_falsely_exclude() {
    // Direct loader: hostile shapes parse without panicking and match
    // nothing real. `id_set` accepts any non-empty entry, so garbage becomes
    // bogus IDs that no fixture actor carries.
    for (key, value) in [
        (AUTOMATION, json!({"actor": AUTOMATION_ACTOR})),
        (RAID, json!(42)),
        (STAGING_GUILDS, json!([["nested"], {"deep": true}])),
        (STAGING_ACTORS, json!(true)),
        (TEST, json!([1.5, "inner\"quote"])),
    ] {
        let rendered = to_env_string(&value).expect("hostile renders");
        let config = ClassifierConfig::from_map(
            &[(key.to_owned(), rendered)]
                .into_iter()
                .collect::<HashMap<String, String>>(),
        );
        for actor in [HUMAN_ACTOR, AUTOMATION_ACTOR, RAID_ACTOR, TEST_ACTOR] {
            assert_eq!(
                verdict_for(&config, actor, GUILD),
                "eligible_human",
                "{key} {value} must not falsely exclude {actor}"
            );
        }
    }
    // Live-layered garbage behaves the same: no crash, no leak, no match.
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![row(GUILD, TEST, json!({"actor": "not-a-real-actor"}), 1)],
    ));
    let snap = live.env_snapshot(Some(GUILD));
    let layered = ClassifierConfig::from_map(&layer_key(&boot_vars(), &snap, TEST));
    assert_eq!(verdict_for(&layered, TEST_ACTOR, GUILD), "eligible_human");
    assert_eq!(verdict_for(&layered, HUMAN_ACTOR, GUILD), "eligible_human");
    let other = ClassifierConfig::from_map(&layer_key(
        &boot_vars(),
        &live.env_snapshot(Some(OTHER_GUILD)),
        TEST,
    ));
    assert_eq!(
        verdict_for(&other, HUMAN_ACTOR, OTHER_GUILD),
        "eligible_human"
    );
}

// ---------------------------------------------------------------------------
// (9) generations: new facts use the new config; history is never rewritten.
// ---------------------------------------------------------------------------

fn fact_row(id: i64, actor: &str, classification: &str) -> FactRow {
    FactRow {
        id,
        guild_id: GUILD.to_owned(),
        event_type: "message_created".to_owned(),
        source_event_id: format!("msg-{id}"),
        actor_id: Some(actor.to_owned()),
        occurred_at: "2026-09-02T10:00:00.000Z".to_owned(),
        source: "channel:general".to_owned(),
        classifier_version: "community-v1".to_owned(),
        classification: classification.to_owned(),
        matched_rule: "fixture".to_owned(),
        metadata: None,
        idempotency_key: format!("key-{id}"),
    }
}

#[test]
fn new_generations_classify_new_facts_without_rewriting_history() {
    let (mut writer, live) = live_channel();
    // Generation 1: no exclusions. The actor is human; the fact row pins it.
    writer.publish(&snapshot(1, vec![]));
    let snap1 = live.env_snapshot(Some(GUILD));
    let config1 = ClassifierConfig::from_map(&layer_five(&boot_vars(), &snap1));
    assert_eq!(verdict_for(&config1, TEST_ACTOR, GUILD), "eligible_human");
    let history = vec![fact_row(1, TEST_ACTOR, "eligible_human")];
    let published1 = live.snapshot();

    // Generation 2: the actor is excluded. New facts use the new verdict.
    writer.publish(&snapshot(2, vec![row(GUILD, TEST, json!([TEST_ACTOR]), 1)]));
    let snap2 = live.env_snapshot(Some(GUILD));
    let config2 = ClassifierConfig::from_map(&layer_five(&boot_vars(), &snap2));
    assert_eq!(verdict_for(&config2, TEST_ACTOR, GUILD), "test");

    // The old generation is untouched: same config verdict, same snapshot
    // contents, same stored fact rows. Nothing rewrites history.
    assert_eq!(
        verdict_for(&config1, TEST_ACTOR, GUILD),
        "eligible_human",
        "old configs never mutate under a new publish"
    );
    assert_eq!(published1.get(GUILD, TEST), None);
    assert_eq!(live.snapshot().get(GUILD, TEST), Some(&json!([TEST_ACTOR])));
    assert!(!Arc::ptr_eq(&published1, &live.snapshot()));
    assert_eq!(history[0].classification, "eligible_human");
    assert_eq!(history[0].actor_id.as_deref(), Some(TEST_ACTOR));

    // The scorecard still builds over the historical rows exactly as stored:
    // old facts keep old verdicts even after the exclusion lands.
    let inputs = ScorecardInputs {
        guild_id: GUILD.to_owned(),
        classifier_version: "community-v1".to_owned(),
        week_start: "2026-08-31T00:00:00.000Z".to_owned(),
        week_end: "2026-09-07T00:00:00.000Z".to_owned(),
        watermark: 1,
        generated_at: "2026-09-07T06:15:00.000Z".to_owned(),
        recommendations_enabled: true,
        correction_cycles: 0,
        facts: history,
        coverage: COMMUNITY_FACT_TYPES
            .iter()
            .map(|stream| StreamCoverage {
                stream: (*stream).to_owned(),
                covered_from: "2026-08-31T00:00:00.000Z".to_owned(),
                covered_through: "2026-09-07T00:00:00.000Z".to_owned(),
            })
            .collect(),
        duplicate_source_ids: false,
        existing_revision_count: 0,
        existing_scorecard_json: None,
    };
    let outcome = build_scorecard(inputs).expect("historical facts still build");
    let scorecard = outcome.scorecard.expect("fresh build");
    assert_eq!(
        scorecard.exclusion_counts["message_created"]["eligible_human"], 1,
        "history keeps its human verdict after the exclusion lands"
    );
    assert_eq!(scorecard.exclusion_counts["message_created"]["test"], 0);
}

// ---------------------------------------------------------------------------
// (10) capture and award rules preserved (controls via the funnel seam).
// ---------------------------------------------------------------------------

type RecordedMessage = (u64, u64, bool, Option<u64>, bool);

#[derive(Debug, Clone, Default)]
struct RecordingSink(Arc<Mutex<Vec<RecordedMessage>>>);

impl RecordingSink {
    fn recorded(&self) -> Vec<RecordedMessage> {
        self.0.lock().expect("recorded lock").clone()
    }
}

impl FactsSink for RecordingSink {
    fn record_member_join(&self, _fact: MemberJoinFact<'_>) {}

    fn record_rules_accepted(&self, _fact: RulesAcceptedFact<'_>) {}

    fn record_message(&self, fact: MessageFact<'_>) {
        self.0.lock().expect("recorded lock").push((
            fact.guild_id,
            fact.member_id,
            fact.is_bot,
            fact.webhook_id,
            fact.is_staff_automation,
        ));
    }

    fn record_voice_started(&self, _fact: VoiceStartedFact<'_>) -> Option<String> {
        None
    }

    fn record_voice_ended(&self, _fact: VoiceEndedFact<'_>) {}
}

fn message_input(
    member_id: u64,
    is_bot: bool,
    webhook_id: Option<u64>,
    is_staff_automation: bool,
) -> MessageInput {
    MessageInput {
        guild_id: 100,
        member_id,
        is_bot,
        message_id: Some(format!("msg-{member_id}")),
        webhook_id,
        is_staff_automation,
        channel_id: 200,
        channel_class: ChannelClass::Other,
        capture_only: false,
        occurred_at: Some("2026-09-02T10:00:00.000Z".to_owned()),
        content_length: None,
        has_embed_or_attachment: false,
    }
}

#[test]
fn control_capture_records_everything_while_awards_gate_only_bot_webhook_staff() {
    let sink = RecordingSink::default();
    let handlers = FunnelHandlers::new(MemStore::new(), Some(NoopLeveling), Some(sink.clone()));

    // Bots, webhooks and staff automation: captured, never funnel-counted.
    assert!(handlers
        .on_message(message_input(1, true, None, false))
        .event
        .is_none());
    assert!(handlers
        .on_message(message_input(2, false, Some(9), false))
        .event
        .is_none());
    assert!(handlers
        .on_message(message_input(3, false, None, true))
        .event
        .is_none());
    // Raid/staging/test actors are NOT funnel-gated today (MessageInput
    // carries no such flags): their messages still capture AND still produce
    // funnel events. This slice must not change that.
    assert!(handlers
        .on_message(message_input(4, false, None, false))
        .event
        .is_some());
    assert!(handlers
        .on_message(message_input(5, false, None, false))
        .event
        .is_some());

    // Capture never filters: all five facts recorded with actors intact.
    let recorded = sink.recorded();
    assert_eq!(recorded.len(), 5);
    assert_eq!(
        recorded.iter().map(|row| row.1).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5],
        "excluded or not, the actor identity reaches the sink"
    );
}

// ---------------------------------------------------------------------------
// (11) privacy preserved: exclusions never anonymize (controls).
// ---------------------------------------------------------------------------

#[test]
fn control_excluded_actors_keep_identity_for_downstream_privacy_gating() {
    // The classifier verdict travels with the actor; it never strips the
    // identity the store-level privacy gate (`record_fact`) suppresses on.
    // Pin through the funnel seam: the sink sees the full member id.
    let sink = RecordingSink::default();
    let handlers = FunnelHandlers::new(MemStore::new(), Some(NoopLeveling), Some(sink.clone()));
    handlers.on_message(message_input(4242, false, None, false));
    let recorded = sink.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].1, 4242);

    // `classify` takes no privacy input: the same actor/guild/flags always
    // yield the same verdict. Privacy lives in the store, not the classifier.
    let config = ClassifierConfig::from_map(
        &[(TEST.to_owned(), TEST_ACTOR.to_owned())]
            .into_iter()
            .collect(),
    );
    let first = classify(&config, &input_for(TEST_ACTOR, GUILD));
    let second = classify(&config, &input_for(TEST_ACTOR, GUILD));
    assert_eq!(first, second);
    assert_eq!(first.classification, "test");
}

// ---------------------------------------------------------------------------
// (12) refresh atomicity and failure preservation.
// ---------------------------------------------------------------------------

#[test]
fn one_snapshot_publishes_and_serves_all_five_atomically() {
    let (mut writer, live) = live_channel();
    let report = writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, AUTOMATION, json!([AUTOMATION_ACTOR]), 1),
            row(GUILD, RAID, json!([RAID_ACTOR]), 2),
            row(GUILD, STAGING_GUILDS, json!([STAGING_GUILD]), 3),
            row(GUILD, STAGING_ACTORS, json!([STAGING_ACTOR]), 4),
            row(GUILD, TEST, json!([TEST_ACTOR]), 5),
        ],
    ));
    // All five report hot in the one report — never a mixed subset — and all
    // five serve together from the one published snapshot.
    assert_eq!(report.hot.len(), 5, "{report:?}");
    assert!(report.cold.is_empty(), "{report:?}");
    let snap = live.env_snapshot(Some(GUILD));
    for key in FIVE {
        assert!(snap.contains_key(key), "{key} missing from {snap:?}");
    }
    let layered = ClassifierConfig::from_map(&layer_five(&boot_vars(), &snap));
    assert_eq!(
        verdict_for(&layered, AUTOMATION_ACTOR, GUILD),
        "staff_automation"
    );
    assert_eq!(verdict_for(&layered, RAID_ACTOR, GUILD), "raid");
    assert_eq!(verdict_for(&layered, STAGING_ACTOR, GUILD), "staging");
    assert_eq!(verdict_for(&layered, TEST_ACTOR, GUILD), "test");
}

#[test]
fn control_a_poll_that_does_not_publish_preserves_the_snapshot() {
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(1, vec![row(GUILD, TEST, json!([TEST_ACTOR]), 1)]));
    let published = live.snapshot();
    // Identical marks are a no-op: readers keep the same Arc. A failed poll
    // (no publish call at all) preserves by the same path — the writer never
    // clears on failure.
    assert!(!writer.needs_refresh(1, 1));
    let report = writer.publish(&snapshot(1, vec![row(GUILD, TEST, json!([TEST_ACTOR]), 1)]));
    assert!(!report.changed);
    assert!(Arc::ptr_eq(&published, &live.snapshot()));
}

// ---------------------------------------------------------------------------
// (13) bound reads: one snapshot per refresh, offline only.
// ---------------------------------------------------------------------------

#[test]
fn one_snapshot_read_layers_all_five_with_no_database() {
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, AUTOMATION, json!([AUTOMATION_ACTOR]), 1),
            row(GUILD, RAID, json!([RAID_ACTOR]), 2),
            row(GUILD, STAGING_GUILDS, json!([STAGING_GUILD]), 3),
            row(GUILD, STAGING_ACTORS, json!([STAGING_ACTOR]), 4),
            row(GUILD, TEST, json!([TEST_ACTOR]), 5),
        ],
    ));
    // Exactly one `env_snapshot` call — the bound production refreshes share
    // with the human/welcome overlay — carries every exclusion. This test
    // compiles and runs with no `db` feature, no `TestDatabase`, no `sqlx`,
    // and no process-environment reads, so a per-message database query
    // cannot hide behind it.
    let snap = live.env_snapshot(Some(GUILD));
    assert_eq!(
        FIVE.iter().filter(|key| snap.contains_key(**key)).count(),
        5,
        "one snapshot must carry all five: {snap:?}"
    );
}

// ---------------------------------------------------------------------------
// (14) reference truth and verification docs.
// ---------------------------------------------------------------------------

#[test]
fn configuration_reference_lists_the_five_as_live_applied() {
    let doc = read_doc("docs/configuration.md");
    for key in FIVE {
        let row = doc
            .lines()
            .find(|line| line.starts_with(&format!("| `{key}` |")))
            .unwrap_or_else(|| panic!("{key} has no catalog row"));
        assert!(
            row.contains("stored, applied live by runtime refresh (reload-report hot)"),
            "{key} must read live-applied, got: {row}"
        );
        assert!(!row.contains("stored unwired"), "{key}: {row}");
    }
}

#[test]
fn community_jobs_doc_documents_this_offline_verification() {
    let doc = read_doc("docs/community-jobs.md");
    assert!(
        doc.contains("community_exclusion_live_keys_acceptance"),
        "docs/community-jobs.md must document the focused offline command for this file"
    );
    assert!(
        doc.contains("community_exclusion_pipeline_acceptance"),
        "docs/community-jobs.md must document the pipeline twin"
    );
}

// A CommonMark fence line: up to three spaces of indent, then a run of three
// or more backticks or tildes. Returns (marker, run length, remainder).
fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = rest.chars().take_while(|c| *c == marker).count();
    (run >= 3).then(|| (marker, run, &rest[run..]))
}

// Lines a Markdown renderer shows as text. Fenced code blocks (``` or ~~~)
// and HTML comment blocks are not index rows, so a link inside one is not
// discoverable. A fence closes only on the same marker at least as long as
// its opener with nothing but whitespace after it; a backtick opener may not
// carry a backtick in its info string.
fn rendered_lines(text: &str) -> Vec<&str> {
    let mut fence: Option<(char, usize)> = None;
    let mut in_comment = false;
    let mut rendered = Vec::new();
    for line in text.lines() {
        if let Some((open, len)) = fence {
            if fence_marker(line)
                .is_some_and(|(c, run, rest)| c == open && run >= len && rest.trim().is_empty())
            {
                fence = None;
            }
            continue;
        }
        if in_comment {
            in_comment = !line.contains("-->");
            continue;
        }
        if let Some((c, run, rest)) = fence_marker(line) {
            if c == '~' || !rest.contains('`') {
                fence = Some((c, run));
                continue;
            }
        }
        let body = line.trim_start_matches(' ');
        if line.len() - body.len() <= 3 && body.starts_with("<!--") {
            in_comment = !body[4..].contains("-->");
            continue;
        }
        rendered.push(line);
    }
    rendered
}

// Match the manifest row contract, not arbitrary mentions of a link. Only
// follow flat index-*.md section fragments explicitly linked by README: an
// orphan, an unrelated guide, or a broken destination is not discoverability.
fn community_jobs_is_indexed(read: impl Fn(&str) -> Option<String>) -> bool {
    let Some(index) = read("docs/README.md") else {
        return false;
    };
    if read("docs/community-jobs.md").is_none() {
        return false;
    }
    let has_guide_row = |text: &str| {
        rendered_lines(text)
            .into_iter()
            .any(|line| line == "- [Community jobs](community-jobs.md)")
    };
    if has_guide_row(&index) {
        return true;
    }
    let fragment_row = regex::Regex::new(r"^- \[([^\]\n]+)\]\((index-[A-Za-z0-9_.-]+\.md)\)$")
        .expect("index fragment row pattern");
    rendered_lines(&index).into_iter().any(|line| {
        fragment_row.captures(line).is_some_and(|row| {
            !row[1].trim().is_empty()
                && read(&format!("docs/{}", &row[2]))
                    .is_some_and(|fragment| has_guide_row(&fragment))
        })
    })
}

fn community_jobs_fixture_is_indexed(files: &[(&str, &str)]) -> bool {
    community_jobs_is_indexed(|path| {
        files
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, contents)| (*contents).to_owned())
    })
}

#[test]
fn control_verification_doc_stays_discoverable_from_the_index() {
    assert!(
        community_jobs_is_indexed(|path| {
            std::fs::read_to_string(repository_root().join(path)).ok()
        }),
        "the verification doc must exist and stay indexed directly or through a README-linked fragment"
    );
}

#[test]
fn control_docs_index_accepts_the_direct_layout() {
    assert!(community_jobs_fixture_is_indexed(&[
        ("docs/README.md", "- [Community jobs](community-jobs.md)\n"),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
}

#[test]
fn control_docs_index_accepts_a_readme_linked_section_fragment() {
    assert!(community_jobs_fixture_is_indexed(&[
        ("docs/README.md", "- [Community](index-community.md)\n"),
        (
            "docs/index-community.md",
            "# Community\n\n- [Community jobs](community-jobs.md)\n",
        ),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
}

#[test]
fn control_docs_index_rejects_a_missing_community_jobs_row() {
    assert!(!community_jobs_fixture_is_indexed(&[
        ("docs/README.md", "- [Community](index-community.md)\n"),
        ("docs/index-community.md", "# Community\n"),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
}

#[test]
fn control_docs_index_rejects_an_unlinked_fragment() {
    assert!(!community_jobs_fixture_is_indexed(&[
        ("docs/README.md", "# Documentation map\n"),
        (
            "docs/index-community.md",
            "- [Community jobs](community-jobs.md)\n",
        ),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
}

#[test]
fn control_docs_index_rejects_a_broken_guide_destination() {
    for (index, fragment) in [
        ("- [Community jobs](community-jobs.md)\n", ""),
        (
            "- [Community](index-community.md)\n",
            "- [Community jobs](community-jobs.md)\n",
        ),
    ] {
        assert!(!community_jobs_fixture_is_indexed(&[
            ("docs/README.md", index),
            ("docs/index-community.md", fragment),
        ]));
    }
}

#[test]
fn control_docs_index_rejects_a_broken_fragment_destination() {
    assert!(!community_jobs_fixture_is_indexed(&[
        ("docs/README.md", "- [Community](index-missing.md)\n"),
        (
            "docs/index-community.md",
            "- [Community jobs](community-jobs.md)\n",
        ),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
}

#[test]
fn control_docs_index_rejects_a_missing_readme() {
    assert!(!community_jobs_fixture_is_indexed(&[
        (
            "docs/index-community.md",
            "- [Community jobs](community-jobs.md)\n",
        ),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
}

#[test]
fn control_docs_index_rejects_rows_inside_fenced_code_blocks() {
    let fragment = "- [Community jobs](community-jobs.md)\n";
    for readme in [
        "```\n- [Community jobs](community-jobs.md)\n```\n",
        "~~~\n- [Community jobs](community-jobs.md)\n~~~\n",
        "```md\n- [Community](index-community.md)\n```\n",
        "~~~\n- [Community](index-community.md)\n~~~\n",
    ] {
        assert!(
            !community_jobs_fixture_is_indexed(&[
                ("docs/README.md", readme),
                ("docs/index-community.md", fragment),
                ("docs/community-jobs.md", "# Community jobs\n"),
            ]),
            "fenced README row must not count: {readme}"
        );
    }
    assert!(
        !community_jobs_fixture_is_indexed(&[
            ("docs/README.md", "- [Community](index-community.md)\n"),
            (
                "docs/index-community.md",
                "```\n- [Community jobs](community-jobs.md)\n```\n",
            ),
            ("docs/community-jobs.md", "# Community jobs\n"),
        ]),
        "fenced fragment guide row must not count"
    );
    // Fence closing follows CommonMark: a shorter run, a different marker or an
    // info string does not close; a longer run does.
    let guide = "- [Community jobs](community-jobs.md)";
    for readme in [
        format!("````\n```\n{guide}\n````\n"),
        format!("```\n```md\n{guide}\n```\n"),
        format!("~~~~\n~~~\n{guide}\n~~~~\n"),
        format!("~~~\n~~~md\n{guide}\n~~~\n"),
        format!("```\n~~~\n{guide}\n~~~\n"),
        format!("```md\n{guide}\n"),
    ] {
        assert!(
            !community_jobs_fixture_is_indexed(&[
                ("docs/README.md", &readme),
                ("docs/community-jobs.md", "# Community jobs\n"),
            ]),
            "row still inside an unclosed fence must not count: {readme}"
        );
    }
    for readme in [
        format!("```\nexample\n````\n{guide}\n"),
        format!("```\nexample\n```   \n{guide}\n"),
        format!("   ```\nexample\n   ```\n{guide}\n"),
        // Four spaces of indent is an indented code line, not a fence.
        format!("    ```\n\n{guide}\n"),
    ] {
        assert!(
            community_jobs_fixture_is_indexed(&[
                ("docs/README.md", &readme),
                ("docs/community-jobs.md", "# Community jobs\n"),
            ]),
            "row after a properly closed fence is rendered: {readme}"
        );
    }
    // A row after a closed fence is rendered again.
    assert!(community_jobs_fixture_is_indexed(&[
        (
            "docs/README.md",
            "```\nexample\n```\n- [Community jobs](community-jobs.md)\n",
        ),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
}

#[test]
fn control_docs_index_rejects_rows_inside_html_comments() {
    let guide = "- [Community jobs](community-jobs.md)";
    let fragment_row = "- [Community](index-community.md)";
    let fragment = format!("{guide}\n");
    let hidden = |row: &str| format!("<!--\n{row}\n-->\n");
    assert!(!community_jobs_fixture_is_indexed(&[
        ("docs/README.md", &hidden(guide)),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
    assert!(!community_jobs_fixture_is_indexed(&[
        ("docs/README.md", &hidden(fragment_row)),
        ("docs/index-community.md", &fragment),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
    assert!(!community_jobs_fixture_is_indexed(&[
        ("docs/README.md", &format!("{fragment_row}\n")),
        ("docs/index-community.md", &hidden(guide)),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
    // A comment opened on the same line as other text still hides what follows.
    assert!(!community_jobs_fixture_is_indexed(&[
        ("docs/README.md", &format!("<!-- hidden\n\n{guide}\n-->\n"),),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
    // Rows after a closed comment (single- or multi-line) are rendered again.
    for readme in [
        format!("<!-- note -->\n{guide}\n"),
        format!("<!--\nnote\n-->\n{guide}\n"),
    ] {
        assert!(community_jobs_fixture_is_indexed(&[
            ("docs/README.md", &readme),
            ("docs/community-jobs.md", "# Community jobs\n"),
        ]));
    }
    // A dotted flat fragment name is a valid link target.
    assert!(community_jobs_fixture_is_indexed(&[
        ("docs/README.md", "- [Community](index-community.v2.md)\n"),
        ("docs/index-community.v2.md", &fragment),
        ("docs/community-jobs.md", "# Community jobs\n"),
    ]));
}

#[test]
fn control_docs_index_rejects_non_manifest_links() {
    for line in [
        "See [Community jobs](community-jobs.md).",
        "- [Community jobs](community-jobs.md) trailing text",
        "- [Community jobs](missing.md)",
        "See [Community](index-community.md).",
        "- [ ](index-community.md)",
        "- [Community](../docs/index-community.md)",
        "- [Community](https://example.invalid/index-community.md)",
        // Boundary anchors of the fragment row: prefix-only and suffix-only.
        "See - [Community](index-community.md)",
        "- [Community](index-community.md) trailing text",
    ] {
        assert!(
            !community_jobs_fixture_is_indexed(&[
                ("docs/README.md", line),
                (
                    "docs/index-community.md",
                    "- [Community jobs](community-jobs.md)\n",
                ),
                ("docs/community-jobs.md", "# Community jobs\n"),
            ]),
            "not a discoverable manifest row: {line}"
        );
    }
}

#[test]
fn control_valid_writes_validate_as_hot_without_new_refusal_codes() {
    // The five need no new write validation: any JSON value stores (the
    // infallible `id_set` reader accepts every rendering), deletes validate,
    // and only the shared NUL/actor guards refuse.
    for key in FIVE {
        for value in [
            json!("a-1,b-2"),
            json!(["a-1", "b-2"]),
            json!(""),
            json!([]),
            json!(42),
        ] {
            let write = validate_write(GUILD, key, Some(value.clone()), ACTOR)
                .unwrap_or_else(|err| panic!("valid write refused {key} {value}: {err:?}"));
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
    }
}

// ---------------------------------------------------------------------------
// (15) controls: already-wired keys stay live and unchanged.
// ---------------------------------------------------------------------------

#[test]
fn control_human_welcome_channels_stay_hot_wired_and_live() {
    use two_bot_core::ScorecardGates;

    for key in [
        "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
        "TWO_COMMUNITY_WELCOME_CHANNEL_IDS",
    ] {
        assert_eq!(classify_key(key), Some(SettingClass::Hot), "{key}");
        assert!(HOT_WIRED.contains(&key), "{key}");
    }
    const HUMAN: &str = "200000000000000001";
    const WELCOME: &str = "300000000000000001";
    let (mut writer, live) = live_channel();
    let report = writer.publish(&snapshot(
        1,
        vec![
            row(GUILD, "TWO_COMMUNITY_HUMAN_CHANNEL_IDS", json!([HUMAN]), 1),
            row(
                GUILD,
                "TWO_COMMUNITY_WELCOME_CHANNEL_IDS",
                json!([WELCOME]),
                2,
            ),
        ],
    ));
    assert_eq!(report.hot.len(), 2, "{report:?}");
    let snap = live.env_snapshot(Some(GUILD));
    let config = ClassifierConfig::from_map(&layer_key(
        &HashMap::new(),
        &snap,
        "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
    ));
    // Layering only the human key still classifies the human channel; the
    // welcome assertion layers both through the same snapshot map.
    assert_eq!(
        config.channel_class(200_000_000_000_000_001),
        ChannelClass::Human
    );
    let mut both = HashMap::new();
    for key in [
        "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
        "TWO_COMMUNITY_WELCOME_CHANNEL_IDS",
    ] {
        if let Some(value) = snap.get(key) {
            both.insert(key.to_owned(), value.clone());
        }
    }
    let config = ClassifierConfig::from_map(&both);
    assert_eq!(
        config.channel_class(300_000_000_000_000_001),
        ChannelClass::Welcome
    );
    // The scorecard gates this slice must not disturb stay hot-wired.
    for key in [
        "TWO_COMMUNITY_SCORECARD",
        "TWO_COMMUNITY_RECOMMENDATIONS",
        "TWO_COMMUNITY_CORRECTION_CYCLES",
        "TWO_PRESENCE_PROBE",
        "TWO_INACTIVITY_DAYS",
    ] {
        assert!(HOT_WIRED.contains(&key), "{key}");
    }
    let gates = ScorecardGates::from_map(&HashMap::new()).expect("defaults parse");
    assert!(!gates.enabled);
}

// ---------------------------------------------------------------------------
// (16) AX: stable IDs, bounded refusals, no dumps, no per-actor labels.
// ---------------------------------------------------------------------------

#[test]
fn refresh_reports_carry_stable_key_ids_for_the_five() {
    let (mut writer, _) = live_channel();
    let report = writer.publish(&snapshot(
        1,
        FIVE.iter()
            .enumerate()
            .map(|(i, key)| row(GUILD, key, json!(["x-1"]), i as i64 + 1))
            .collect(),
    ));
    let mut seen: Vec<&str> = report
        .hot
        .iter()
        .map(|change| change.key.as_str())
        .collect();
    seen.sort_unstable();
    let mut expected = FIVE.to_vec();
    expected.sort_unstable();
    assert_eq!(
        seen, expected,
        "hot report keys are the stable IDs: {report:?}"
    );
}

#[test]
fn write_refusals_are_bounded_and_never_echo_submitted_values() {
    for key in FIVE {
        let probe = format!("{key}-actor-9Z9Z9Z");
        // NUL is refused without echoing the payload.
        let err = validate_write(GUILD, key, Some(json!(format!("a\0{probe}"))), ACTOR)
            .expect_err("NUL must refuse");
        assert_eq!(err, WriteRefusal::NullCharacter);
        assert!(
            !err.to_string().contains(&probe),
            "refusal must not echo the value: {err}"
        );
        assert!(
            !format!("{err:?}").contains(&probe),
            "debug refusal must not echo the value: {err:?}"
        );
        // Missing actor is refused without echoing the payload.
        let err = validate_write(GUILD, key, Some(json!([probe.clone()])), "")
            .expect_err("missing actor must refuse");
        assert_eq!(err, WriteRefusal::MissingActor);
        assert!(!err.to_string().contains(&probe), "{err}");
    }
}

#[test]
fn reference_docs_contain_no_actor_snowflakes_or_per_actor_labels() {
    let snowflake = regex::Regex::new(r"\b[0-9]{17,20}\b").expect("snowflake pattern");
    let config = read_doc("docs/configuration.md");
    for key in FIVE {
        let row = config
            .lines()
            .find(|line| line.starts_with(&format!("| `{key}` |")))
            .unwrap_or_else(|| panic!("{key} has no catalog row"));
        assert!(
            !snowflake.is_match(row),
            "{key} row must not embed live IDs: {row}"
        );
    }
    // Settings are not metrics: the five must never appear as metric names
    // or high-cardinality label values.
    let metrics = read_doc("docs/metrics.md");
    for key in FIVE {
        assert!(
            !metrics.contains(key),
            "{key} must not gain per-actor metrics"
        );
    }
}
