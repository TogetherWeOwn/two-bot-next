//! Protected acceptance: boot one real command composition, then change
//! stored exclusions. The paired Builder must incorporate this file unchanged.
//! Breaks caught: attendance using boot-only classification, a union instead
//! of replacement, cached-at-ack settings, empty/delete confusion, guild bleed.
#![cfg(test)]

use super::{isolated, isolated_with_env, BOOT, FOREIGN, GUILD, KEY, STORED};
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use std::sync::Arc;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{InteractionRouter, OnboardingGates, OnboardingMode, RouterGates};
use two_bot_discord::{ActionExecutor, LevelingRuntime};
use two_bot_testsupport::TestDatabase;

const EVENT: &str = "700000000000000021";
const VERSION: &str = "community-v1";

fn attendance(id: u64, actor: &str, occurrence: &str) -> Interaction {
    let user = json!({"id":actor,"username":"synthetic-human","discriminator":"0"});
    serde_json::from_value(json!({
        "application_id":"1111","authorizing_integration_owners":{"0":GUILD.to_string()},
        "id":id.to_string(),"token":"synthetic-checkin-token","type":2,"version":1,
        "guild_id":GUILD.to_string(),
        "member":{"permissions":(1_u64 << 33).to_string(),"roles":[],"deaf":false,"mute":false,"flags":0,"user":user},
        "data":{"id":"4444","name":"attendance","type":1,
            "options":[{"name":"event-occurrence","type":3,"value":occurrence},
                {"name":"member","type":6,"value":actor}],
            "resolved":{"users":{actor:user}}}
    })).expect("synthetic host self-check-in")
}

fn router() -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD),
        scorecard: true,
        announcements: true,
        automations: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    })
}

fn checkin_responses(actor: &str) -> Vec<ScriptedResponse> {
    let event = || {
        ScriptedResponse::json(
            200,
            json!({"id":EVENT,"guild_id":GUILD.to_string(),"status":1}),
        )
    };
    let member = || ScriptedResponse::json(200, json!({"user":{"id":actor},"roles":[]}));
    vec![
        ScriptedResponse::status(204),
        event(),
        member(),
        event(),
        member(),
        ScriptedResponse::json(200, json!({"id":"99"})),
    ]
}

#[derive(Clone, Copy)]
enum CheckinPath {
    Direct,
    CommandOrdered,
    BootOrdered,
}

async fn live_attendance(path: CheckinPath) {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("disposable test bootstrap required; no fallback");
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable fixture");
    let origin = url.rsplit_once('/').expect("guarded bootstrap URL").0;
    let poll = crate::settings_jobs::job(&format!("{origin}/{}", fixture.name()));
    (poll.action)().await.expect("initial real settings poll");
    let live = crate::settings_jobs::live().expect("registered live settings");
    let store = two_bot_cutover::settings::SettingsStore::new(fixture.pool());

    // Each observation is a unique occurrence, avoiding attendance dedupe.
    // One runtime must observe all transitions, not be rebuilt after each poll.
    let cases = [
        ("boot", BOOT[0], None, "test", "configured_test_actor"),
        (
            "stored",
            STORED[0],
            Some(json!([STORED[0]])),
            "test",
            "configured_test_actor",
        ),
        (
            "replacement",
            BOOT[0],
            Some(json!([STORED[0]])),
            "eligible_human",
            "no_exclusion_matched",
        ),
        (
            "empty",
            BOOT[0],
            Some(json!([])),
            "eligible_human",
            "no_exclusion_matched",
        ),
        ("deleted", BOOT[0], None, "test", "configured_test_actor"),
        (
            "foreign-only",
            STORED[0],
            None,
            "eligible_human",
            "no_exclusion_matched",
        ),
    ];
    let script = cases
        .iter()
        .flat_map(|(_, actor, _, _, _)| checkin_responses(actor))
        .collect();
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let executor =
        ActionExecutor::with_proxy("synthetic-test-token".to_owned(), Some(mock.origin()))
            .expect("loopback REST executor");
    let onboarding = OnboardingGates {
        mode: OnboardingMode::Legacy,
        dry_run: false,
    };
    // Keep the command constructor's regressions independent of the separate
    // production constructor used by ordered gateway completion.
    let commands = (!matches!(path, CheckinPath::BootOrdered)).then(|| {
        let leveling = LevelingRuntime::new(
            fixture.pool().clone(),
            Arc::new(executor.clone()),
            GUILD,
            onboarding,
        );
        crate::command_runtime::CommandRuntime::build(
            fixture.pool().clone(),
            executor.clone(),
            router(),
            None,
            leveling,
            GUILD,
            None,
            None,
            None,
        )
    });
    let ordered = matches!(path, CheckinPath::BootOrdered).then(|| {
        // Same permitted synthetic identity as the composition fixtures; all
        // HTTP still goes exclusively to MockRest, never to that guild.
        let activation = crate::activation::BootActivation::from_token(
            Some(1545644954272137297),
            Some("MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature"),
        );
        crate::build_interaction_runtime(
            fixture.pool(),
            GUILD,
            &onboarding,
            executor.clone(),
            &activation,
        )
        .expect("valid boot activation and feature gates build ordered runtime")
    });
    let runtime = ordered.as_deref().unwrap_or_else(|| {
        commands
            .as_ref()
            .expect("command-path fixture")
            .test_interactions()
    });
    let mut expected = Vec::new();
    let mut expected_identities = Vec::new();
    for (index, (label, actor, stored, classification, rule)) in cases.into_iter().enumerate() {
        let occurrence = format!("{EVENT}:{label}");
        let interaction = attendance(8000 + index as u64, actor, &occurrence);
        // For the ordered gateway path acknowledge BEFORE the poll, so a
        // classifier frozen by prepare() is observably too early.
        let prepared = if !matches!(path, CheckinPath::Direct) {
            Some(
                runtime
                    .prepare(interaction.clone(), runtime.router.gates().announcements)
                    .await
                    .expect("real acknowledgement path"),
            )
        } else {
            None
        };
        if prepared.is_some() {
            let requests = mock.requests();
            assert_eq!(requests.len(), index * 6 + 1, "ack precedes execution");
            let ack: Value =
                serde_json::from_slice(&requests[index * 6].body).expect("acknowledgement JSON");
            assert_eq!(ack["type"], 5, "real deferred acknowledgement before poll");
        }
        store
            .set(&GUILD.to_string(), KEY, stored.clone(), "synthetic-test")
            .await
            .expect("own-guild setting transition");
        store
            .set(
                &FOREIGN.to_string(),
                KEY,
                Some(json!([STORED[0]])),
                "synthetic-test",
            )
            .await
            .expect("unrelated guild snapshot");
        (poll.action)()
            .await
            .expect("real settings poll publishes transition");
        assert_eq!(
            live.get(&GUILD.to_string(), KEY),
            stored,
            "own transition really published"
        );
        assert_eq!(
            live.get(&FOREIGN.to_string(), KEY),
            Some(json!([STORED[0]])),
            "unrelated snapshot really published"
        );
        let handled = if let Some(prepared) = prepared {
            runtime
                .complete(prepared)
                .await
                .expect("real ordered completion")
        } else {
            runtime
                .handle(&interaction)
                .await
                .expect("real direct check-in")
        };
        assert!(handled, "registered attendance must execute");
        expected_identities.push((
            GUILD.to_string(),
            actor.to_owned(),
            format!("event:{occurrence}"),
        ));
        expected.push((
            format!("{occurrence}:{actor}"),
            classification.to_owned(),
            rule.to_owned(),
            VERSION.to_owned(),
            json!({"eventOccurrenceId":occurrence,"proof":"host_checkin"}).to_string(),
        ));
    }
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT source_event_id, classification, matched_rule, classifier_version, metadata FROM community_facts WHERE event_type = 'event_attended' ORDER BY id"
    ).fetch_all(fixture.pool()).await.expect("persisted runtime attendance verdicts");
    let identities: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT guild_id, actor_id, source FROM community_facts WHERE event_type = 'event_attended' ORDER BY id"
    ).fetch_all(fixture.pool()).await.expect("persisted attendance guild, actor and source");
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log WHERE action = 'event.checkin' AND outcome = 'recorded'")
        .fetch_one(fixture.pool()).await.expect("required host-checkin audit rows");
    let requests = mock.requests();
    mock.shutdown().await;
    drop(ordered);
    drop(commands);
    drop(executor);
    drop(poll);
    fixture
        .close()
        .await
        .expect("teardown before intended RED assertion");

    assert_eq!(
        audits, 6,
        "every accepted check-in retains its mandatory audit"
    );
    assert_eq!(
        requests.len(),
        36,
        "real membership and event pre/post fences remain exercised"
    );
    for request in requests.iter().filter(|request| request.method == "PATCH") {
        let reply: Value = serde_json::from_slice(&request.body).expect("deferred reply JSON");
        assert!(
            reply["content"]
                .as_str()
                .expect("reply content")
                .starts_with("Recorded <@"),
            "successful execution, not a refusal: {reply}"
        );
    }
    // Compare JSON semantically; database serialization need not preserve key order.
    let actual: Vec<_> = rows
        .into_iter()
        .map(|(source, class, rule, version, metadata)| {
            (
                source,
                class,
                rule,
                version,
                serde_json::from_str::<Value>(&metadata).expect("fact metadata"),
            )
        })
        .collect();
    let expected: Vec<_> = expected
        .into_iter()
        .map(|(source, class, rule, version, metadata)| {
            (
                source,
                class,
                rule,
                version,
                serde_json::from_str::<Value>(&metadata).expect("expected metadata"),
            )
        })
        .collect();
    assert_eq!(
        identities, expected_identities,
        "exact attendance attribution"
    );
    assert_eq!(actual, expected, "check-in execution uses published replacement/empty/delete fallback and no foreign-guild overlay");
}

#[tokio::test]
async fn direct_checkin_uses_published_live_exclusions() {
    if !isolated("attendance::direct_checkin_uses_published_live_exclusions") {
        live_attendance(CheckinPath::Direct).await;
    }
}

#[tokio::test]
async fn ordered_checkin_resolves_exclusions_after_acknowledgement() {
    if !isolated("attendance::ordered_checkin_resolves_exclusions_after_acknowledgement") {
        live_attendance(CheckinPath::CommandOrdered).await;
    }
}

// Break caught: remove only build_interaction_runtime's provider installation.
// CommandRuntime::build is deliberately not constructed on this path.
#[tokio::test]
async fn production_ordered_checkin_resolves_exclusions_after_acknowledgement() {
    if !isolated_with_env(
        "attendance::production_ordered_checkin_resolves_exclusions_after_acknowledgement",
        &[
            ("TWO_COMMUNITY_SCORECARD", "1"),
            ("TWO_ANNOUNCEMENTS", "1"),
            ("TWO_AUTOMATIONS", "0"),
            ("TWO_MODERATION", "0"),
            ("TWO_MODERATION_PROTECTED_ROLE_IDS", ""),
            ("TWO_FEED_POLL_SECONDS", "300"),
        ],
    ) {
        live_attendance(CheckinPath::BootOrdered).await;
    }
}
