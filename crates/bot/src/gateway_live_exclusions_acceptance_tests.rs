//! Protected acceptance: use the production overlay and refresh, never a
//! test-local copy. The paired Builder must incorporate this file unchanged.
//! Breaks caught: a missing exclusion overlay arm; message-only refresh;
//! stale stored values after an empty row/deletion; cross-guild config bleed.
#![cfg(test)]

#[path = "attendance_live_exclusions_acceptance_tests.rs"]
mod attendance;

use super::*;
use serde_json::{json, Value};
use std::collections::HashSet;
use two_bot_core::settings::{live_channel, SettingRow, SettingsSnapshot};
use two_bot_core::{classify, ClassifierConfig, ClassifyInput};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 100_000_000_000_000_021;
const FOREIGN: u64 = 100_000_000_000_000_022;
const BOOT: [&str; 4] = [
    "900000000000001001",
    "900000000000001002",
    "900000000000001003",
    "900000000000001004",
];
const STORED: [&str; 4] = [
    "900000000000002001",
    "900000000000002002",
    "900000000000002003",
    "900000000000002004",
];
const KEY: &str = "TWO_COMMUNITY_TEST_ACTOR_IDS";

fn row(guild: &str, key: &str, ids: &[&str]) -> SettingRow {
    SettingRow {
        guild_id: guild.to_owned(),
        key: key.to_owned(),
        value: json!(ids),
        version: 1,
    }
}

fn input(id: &str, guild_scope: bool) -> ClassifyInput {
    ClassifyInput {
        guild_id: if guild_scope {
            id
        } else {
            "100000000000000021"
        }
        .to_owned(),
        actor_id: if guild_scope {
            "900000000000009999"
        } else {
            id
        }
        .to_owned(),
        is_bot: false,
        webhook_id: None,
        is_staff_automation: false,
        is_raid: false,
        is_staging: false,
        is_test: false,
    }
}

macro_rules! overlay_case {
    ($name:ident, $key:literal, $field:ident, $class:literal, $rule:literal, $guild_scope:literal) => {
        #[test]
        fn $name() {
            let deployment = HashMap::from([($key.to_owned(), "boot-id".to_owned())]);
            let (mut writer, live) = live_channel();
            let boot = ClassifierConfig {
                $field: HashSet::from(["boot-id".to_owned()]),
                ..ClassifierConfig::default()
            };
            assert_eq!(classifier_config_from_live(&deployment, "a", &live), boot);

            // One changed key at a time: deleting any production overlay arm
            // independently must fail its own collected test.
            for (revision, own_row, expected_ids) in [
                (
                    1,
                    Some(vec!["stored-1", "stored-2"]),
                    vec!["stored-1", "stored-2"],
                ),
                (2, Some(vec![]), vec![]),
                (3, None, vec!["boot-id"]),
            ] {
                let mut rows = vec![row("b", $key, &["foreign-id"])];
                if let Some(ids) = own_row {
                    rows.push(row("a", $key, &ids));
                }
                writer.publish(&SettingsSnapshot { revision, rows });
                let config = classifier_config_from_live(&deployment, "a", &live);
                assert_eq!(
                    config,
                    ClassifierConfig {
                        $field: expected_ids.iter().map(|id| (*id).to_owned()).collect(),
                        ..ClassifierConfig::default()
                    },
                    "exact replacement, including empty/delete at revision {revision}"
                );
                for id in [
                    "boot-id",
                    "stored-1",
                    "stored-2",
                    "foreign-id",
                    "ordinary-id",
                ] {
                    let verdict = classify(&config, &input(id, $guild_scope));
                    let excluded = expected_ids.contains(&id);
                    assert_eq!(
                        verdict.classification,
                        if excluded { $class } else { "eligible_human" },
                        "revision {revision}, scope {id}"
                    );
                    assert_eq!(
                        verdict.matched_rule,
                        if excluded {
                            $rule
                        } else {
                            "no_exclusion_matched"
                        }
                    );
                }
                let foreign = classifier_config_from_live(&deployment, "b", &live);
                assert_eq!(
                    foreign,
                    ClassifierConfig {
                        $field: HashSet::from(["foreign-id".to_owned()]),
                        ..ClassifierConfig::default()
                    },
                    "unrelated guild retains only its own replacement"
                );
                assert_eq!(
                    classify(&foreign, &input("foreign-id", $guild_scope)).classification,
                    $class
                );
                assert_eq!(
                    classify(&foreign, &input("stored-1", $guild_scope)).classification,
                    "eligible_human"
                );
                assert_eq!(
                    classifier_config_from_live(&deployment, "c", &live),
                    boot,
                    "a guild without a stored row uses boot, not either neighbor"
                );
            }
        }
    };
}

overlay_case!(
    automation_overlay_replaces_empties_deletes_and_isolates,
    "TWO_COMMUNITY_AUTOMATION_ACTOR_IDS",
    automation_actor_ids,
    "staff_automation",
    "configured_automation_actor",
    false
);
overlay_case!(
    raid_overlay_replaces_empties_deletes_and_isolates,
    "TWO_COMMUNITY_RAID_ACTOR_IDS",
    raid_actor_ids,
    "raid",
    "configured_raid_actor",
    false
);
overlay_case!(
    staging_actor_overlay_replaces_empties_deletes_and_isolates,
    "TWO_COMMUNITY_STAGING_ACTOR_IDS",
    staging_actor_ids,
    "staging",
    "configured_staging_scope",
    false
);
overlay_case!(
    staging_guild_overlay_replaces_empties_deletes_and_isolates,
    "TWO_COMMUNITY_STAGING_GUILD_IDS",
    staging_guild_ids,
    "staging",
    "configured_staging_scope",
    true
);
overlay_case!(
    test_actor_overlay_replaces_empties_deletes_and_isolates,
    "TWO_COMMUNITY_TEST_ACTOR_IDS",
    test_actor_ids,
    "test",
    "configured_test_actor",
    false
);

// LIVE is a process-wide OnceLock and refresh reads process environment.
// Re-exec only this test to isolate both, without mutating a parallel suite's
// environment. Inherited LLVM_PROFILE_FILE is intentionally preserved.
fn isolated(name: &str) -> bool {
    const CHILD: &str = "TWO_TEST_LIVE_EXCLUSIONS_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }
    let module = module_path!().split_once("::").expect("test module").1;
    let target = format!("{module}::{name}");
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command.args(["--exact", &target, "--nocapture", "--test-threads=1"]);
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("TWO_COMMUNITY_")) {
        command.env_remove(key);
    }
    let output = command
        .env(CHILD, name)
        .env(KEY, BOOT.join(","))
        .env("TWO_DATABASE_TLS", "local-only")
        .env_remove("TWO_OPS_CONFIG_URL")
        .output()
        .expect("isolated acceptance process");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("running 1 test"),
        "must collect real acceptance: {text}"
    );
    assert!(
        output.status.success(),
        "isolated acceptance failed: {text}"
    );
    true
}

fn user(actor: &str) -> Value {
    json!({"id":actor,"username":"synthetic","discriminator":"0","avatar":null,"bot":false})
}

fn member(actor: &str, pending: bool) -> Value {
    json!({"user":user(actor),"roles":[],"joined_at":"2026-10-20T12:00:00+00:00",
        "deaf":false,"mute":false,"pending":pending,"flags":0})
}

#[derive(Clone, Copy)]
enum Stream {
    Voice,
    Join,
    Rules,
}

impl Stream {
    fn event_type(self) -> &'static str {
        match self {
            Self::Voice => "voice_session_started",
            Self::Join => "member_joined",
            Self::Rules => "rules_accepted",
        }
    }

    fn event(self, guild: u64, actor: &str) -> Event {
        match self {
            Self::Voice => Event::VoiceStateUpdate(Box::new(serde_json::from_value(json!({
                "guild_id":guild.to_string(),"user_id":actor,"channel_id":"300000000000000021",
                "member":member(actor, false),"session_id":"synthetic-voice","deaf":false,"mute":false,
                "self_deaf":false,"self_mute":false,"self_video":false,"suppress":false
            })).expect("voice payload"))),
            Self::Join => {
                let mut value = member(actor, true);
                value["guild_id"] = json!(guild.to_string());
                Event::MemberAdd(Box::new(serde_json::from_value(value).expect("pending join payload")))
            }
            Self::Rules => {
                let mut value = member(actor, false);
                value["guild_id"] = json!(guild.to_string());
                Event::MemberUpdate(Box::new(serde_json::from_value(value).expect("gate-clear payload")))
            }
        }
    }
}

async fn message_free_refresh(stream: Stream) {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("disposable test bootstrap required; no fallback");
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable fixture");
    let bootstrap_origin = url.rsplit_once('/').expect("guarded bootstrap URL").0;
    let fixture_url = format!("{bootstrap_origin}/{}", fixture.name());
    let poll = crate::settings_jobs::job(&fixture_url);
    (poll.action)().await.expect("initial real settings poll");
    let live = crate::settings_jobs::live().expect("registered live reader");
    let pipeline = build_pipeline(vec![], None);
    pipeline.enable_community_facts(fixture.pool().clone());
    let store = two_bot_cutover::settings::SettingsStore::new(fixture.pool());
    let mut expected = Vec::new();
    let mut inserted = 0;

    for phase in 0..4 {
        let value = match phase {
            0 | 3 => None,
            1 => Some(json!(STORED)),
            _ => Some(json!([])),
        };
        store
            .set(&GUILD.to_string(), KEY, value.clone(), "synthetic-test")
            .await
            .expect("settings write/delete");
        (poll.action)()
            .await
            .expect("real poll publishes update/delete");
        assert_eq!(
            live.get(&GUILD.to_string(), KEY),
            value,
            "poll published this phase"
        );
        // Alternate guilds on the SAME pipeline; a correct refresh must not
        // retain the last guild's override. No MessageCreate is sent anywhere.
        for (guild, ordinary_control) in [(GUILD, false), (FOREIGN, false), (GUILD, true)] {
            // Use a second unique member on the last visit (facts dedupe joins
            // and rules per member). It is an ordinary, non-excluded control.
            let actors = if ordinary_control {
                vec![(900000000000004000_u64 + phase as u64).to_string()]
            } else {
                vec![BOOT[phase].to_owned(), STORED[phase].to_owned()]
            };
            for actor in actors {
                if matches!(stream, Stream::Rules) {
                    // Seed pre-update membership, not another funnel dispatch:
                    // the only fact-producing event is pending true -> false.
                    pipeline.cache().update(&Stream::Join.event(guild, &actor));
                }
                let event = stream.event(guild, &actor);
                refresh_channel_classifier_from_poller(&pipeline, &event);
                let requests = pipeline.collect_at(
                    &event,
                    "2026-10-20T12:01:00.000Z",
                    two_bot_discord::MessageEligibility::default(),
                );
                pipeline
                    .drain(requests)
                    .await
                    .expect("ordered pipeline drain");
                inserted += pipeline
                    .drain_facts()
                    .await
                    .expect("persist classified fact");
                let boot_excluded = BOOT.contains(&actor.as_str());
                let stored_excluded = STORED.contains(&actor.as_str());
                let class = if guild == FOREIGN || phase == 0 || phase == 3 {
                    if boot_excluded {
                        "test"
                    } else {
                        "eligible_human"
                    }
                } else if phase == 1 && stored_excluded {
                    "test"
                } else {
                    "eligible_human"
                };
                expected.push((
                    guild.to_string(),
                    actor,
                    stream.event_type().to_owned(),
                    class.to_owned(),
                ));
            }
        }
    }
    let actual: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT guild_id, actor_id, event_type, classification FROM community_facts ORDER BY guild_id, actor_id, event_type"
    ).fetch_all(fixture.pool()).await.expect("read persisted verdicts");
    expected.sort();
    drop(poll);
    drop(pipeline);
    fixture
        .close()
        .await
        .expect("teardown disposable database before RED assertion");
    assert_eq!(inserted, expected.len(), "one fact per fresh gateway event");
    assert_eq!(
        actual, expected,
        "stored/empty/delete and cross-guild refresh without MessageCreate"
    );
}

#[tokio::test]
async fn voice_refreshes_exclusions_without_messages() {
    if !isolated("voice_refreshes_exclusions_without_messages") {
        message_free_refresh(Stream::Voice).await;
    }
}

#[tokio::test]
async fn member_add_refreshes_exclusions_without_messages() {
    if !isolated("member_add_refreshes_exclusions_without_messages") {
        message_free_refresh(Stream::Join).await;
    }
}

#[tokio::test]
async fn rules_update_refreshes_exclusions_without_messages() {
    if !isolated("rules_update_refreshes_exclusions_without_messages") {
        message_free_refresh(Stream::Rules).await;
    }
}
