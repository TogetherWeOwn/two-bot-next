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
    isolated_with_env(name, &[])
}

fn isolated_with_env(name: &str, vars: &[(&str, &str)]) -> bool {
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
        .envs(vars.iter().copied())
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
    pipeline
        .enable_community_facts(fixture.pool().clone())
        .expect("healthy fixture arming");
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
                let requests = pipeline
                    .collect_at(
                        &event,
                        "2026-10-20T12:01:00.000Z",
                        two_bot_discord::MessageEligibility::default(),
                    )
                    .expect("healthy collect");
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

// Break caught: delete only MemberRemove from the production refresh match.
// Every voice session opens BEFORE the poll; no other gateway event can refresh
// the shared classifier between publication and MemberRemove's end fact.
#[tokio::test]
async fn member_remove_closes_voice_with_fresh_exclusions() {
    if isolated("member_remove_closes_voice_with_fresh_exclusions") {
        return;
    }
    const CHANNEL: &str = "300000000000000021";
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("disposable test bootstrap required; no fallback");
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable fixture");
    let origin = url.rsplit_once('/').expect("guarded bootstrap URL").0;
    let poll = crate::settings_jobs::job(&format!("{origin}/{}", fixture.name()));
    (poll.action)().await.expect("initial real settings poll");
    let live = crate::settings_jobs::live().expect("registered live reader");
    let store = two_bot_cutover::settings::SettingsStore::new(fixture.pool());
    let pipeline = build_pipeline(vec![], None);
    pipeline
        .enable_community_facts(fixture.pool().clone())
        .expect("healthy fixture arming");
    let mut expected = Vec::new();
    let mut inserted = 0;

    // Literal start/end verdicts deliberately differ for every transition
    // under test. Neighbor rows never supply the event guild's exclusions.
    let cases = [
        (
            "stored-only",
            GUILD,
            STORED[0],
            None,
            Some(json!([STORED[0]])),
            None,
            "eligible_human",
            "test",
        ),
        (
            "replace-boot",
            GUILD,
            BOOT[0],
            None,
            Some(json!([STORED[0]])),
            None,
            "test",
            "eligible_human",
        ),
        (
            "update-old",
            GUILD,
            STORED[1],
            Some(json!([STORED[1]])),
            Some(json!([STORED[2]])),
            None,
            "test",
            "eligible_human",
        ),
        (
            "update-new",
            GUILD,
            STORED[2],
            Some(json!([STORED[1]])),
            Some(json!([STORED[2]])),
            None,
            "eligible_human",
            "test",
        ),
        (
            "empty",
            GUILD,
            BOOT[1],
            None,
            Some(json!([])),
            None,
            "test",
            "eligible_human",
        ),
        (
            "delete-boot",
            GUILD,
            BOOT[2],
            Some(json!([])),
            None,
            None,
            "eligible_human",
            "test",
        ),
        (
            "delete-stored",
            GUILD,
            STORED[3],
            Some(json!([STORED[3]])),
            None,
            None,
            "test",
            "eligible_human",
        ),
        (
            "foreign-only",
            FOREIGN,
            STORED[0],
            None,
            None,
            Some(json!([STORED[0]])),
            "eligible_human",
            "eligible_human",
        ),
        (
            "own-after-foreign",
            GUILD,
            "900000000000002005",
            None,
            Some(json!(["900000000000002005"])),
            Some(json!([])),
            "eligible_human",
            "test",
        ),
        (
            "foreign-boot",
            FOREIGN,
            BOOT[0],
            None,
            None,
            Some(json!([])),
            "test",
            "test",
        ),
        (
            "ordinary-control",
            GUILD,
            "900000000000009999",
            None,
            Some(json!([STORED[0]])),
            Some(json!(["900000000000009999"])),
            "eligible_human",
            "eligible_human",
        ),
    ];
    // Each guild/member pair is new: retained gateway voice-cache entries from
    // an earlier departure must not turn a later open into a same-channel noop.
    let mut members = HashSet::new();
    for (index, (label, guild, actor, before, after, neighbor, start_class, end_class)) in
        cases.into_iter().enumerate()
    {
        assert!(
            members.insert((guild, actor)),
            "{label}: fresh session actor"
        );
        let other_guild = if guild == GUILD { FOREIGN } else { GUILD };
        store
            .set(&guild.to_string(), KEY, before, "synthetic-test")
            .await
            .expect("pre-open own-guild setting");
        store
            .set(
                &other_guild.to_string(),
                KEY,
                neighbor.clone(),
                "synthetic-test",
            )
            .await
            .expect("independent neighbor setting");
        (poll.action)().await.expect("publish pre-open snapshot");
        let started = format!("2026-10-20T12:{index:02}:00.000Z");
        let ended = format!("2026-10-20T12:{index:02}:30.000Z");
        let join = Stream::Voice.event(guild, actor);
        refresh_channel_classifier_from_poller(&pipeline, &join);
        let requests = pipeline
            .collect_at(
                &join,
                &started,
                two_bot_discord::MessageEligibility::default(),
            )
            .expect("healthy collect");
        pipeline
            .drain(requests)
            .await
            .expect("open fresh real voice session");
        inserted += pipeline.drain_facts().await.expect("persist start fact");

        store
            .set(&guild.to_string(), KEY, after.clone(), "synthetic-test")
            .await
            .expect("post-open replacement, empty or deletion");
        (poll.action)().await.expect("publish changed exclusions");
        assert_eq!(
            live.get(&guild.to_string(), KEY),
            after,
            "{label}: own snapshot"
        );
        assert_eq!(
            live.get(&other_guild.to_string(), KEY),
            neighbor,
            "{label}: neighbor snapshot"
        );

        let leave = Event::MemberRemove(
            serde_json::from_value(json!({"guild_id":guild.to_string(),"user":user(actor)}))
                .expect("real MemberRemove payload"),
        );
        refresh_channel_classifier_from_poller(&pipeline, &leave);
        let requests = pipeline
            .collect_at(
                &leave,
                &ended,
                two_bot_discord::MessageEligibility::default(),
            )
            .expect("healthy collect");
        pipeline
            .drain(requests)
            .await
            .expect("ordered member-removal drain");
        inserted += pipeline
            .drain_facts()
            .await
            .expect("persist voice end fact");

        let key = format!("{guild}:{actor}:{started}:{CHANNEL}");
        for (event_type, at, class, metadata) in [
            (
                "voice_session_started",
                &started,
                start_class,
                json!({"sessionKey":key,"channelId":CHANNEL}),
            ),
            (
                "voice_session_ended",
                &ended,
                end_class,
                json!({"sessionKey":key,"channelId":CHANNEL,"startedAt":started,
                    "durationSeconds":30,"startKnown":true}),
            ),
        ] {
            expected.push(json!({
                "guild_id":guild.to_string(),"actor_id":actor,"event_type":event_type,
                "source_event_id":key,"source":format!("channel:{CHANNEL}"),"occurred_at":at,
                "classification":class,"classifier_version":"community-v1",
                "matched_rule":if class == "test" { "configured_test_actor" } else { "no_exclusion_matched" },
                "metadata":metadata
            }));
        }
    }
    // Include starts and ends: departure notifications alone cannot satisfy this
    // oracle, nor can an end without its correct session key and known duration.
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'guild_id', guild_id, 'actor_id', actor_id, 'event_type', event_type,
            'source_event_id', source_event_id, 'source', source, 'occurred_at', occurred_at,
            'classification', classification, 'classifier_version', classifier_version,
            'matched_rule', matched_rule, 'metadata', metadata::jsonb)::text
         FROM community_facts ORDER BY id",
    )
    .fetch_all(fixture.pool())
    .await
    .expect("read persisted voice verdicts");
    drop(pipeline);
    drop(poll);
    fixture
        .close()
        .await
        .expect("teardown before intended RED assertion");
    let actual: Vec<Value> = rows
        .iter()
        .map(|row| serde_json::from_str(row).expect("stored fact JSON"))
        .collect();
    assert_eq!(inserted, 22, "one fresh start and one durable end per case");
    assert_eq!(actual, expected, "MemberRemove itself refreshes end classification across replacement/update/empty/delete and guild switches");
}
