//! Protected acceptance: saved state is not current prefix-dispatch eligibility.
//! Only external Discord HTTP is doubled. Flags enter through the existing
//! effective gateway configuration, not a new environment reader or renderer.

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use two_bot_core::{
    custom_command_store,
    custom_commands::{
        accepted_text_trigger, builtin_command_names, reserved_command_names, StoredCommand,
    },
};
use two_bot_testsupport::TestDatabase;

#[derive(Clone, Copy, Debug)]
struct Flags {
    automations: bool,
    text: bool,
    voice: bool,
    assistant: bool,
}

fn configured_runtime(pool: PgPool, origin: String, flags: Flags) -> Arc<CommandRuntime> {
    let mut router_gates = gates(flags.automations, false);
    router_gates.voice = flags.voice;
    router_gates.voice_assistant = flags.assistant;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(router_gates),
        GUILD,
        flags.automations,
    );
    let vars = HashMap::from([
        (
            "TWO_AUTOMATIONS".to_owned(),
            u8::from(flags.automations).to_string(),
        ),
        (
            "TWO_TEXT_COMMANDS".to_owned(),
            u8::from(flags.text).to_string(),
        ),
        ("TWO_AUTOMOD".to_owned(), "0".to_owned()),
    ]);
    runtime.initialize_custom_commands_for_test(
        crate::gateway_commands::GatewayCommandConfig::from_map_for_activation(GUILD, &vars, false)
            .expect("same effective feature parser as the live gateway"),
    );
    runtime
}

fn invocation(id: u64, name: &str, view: Option<&str>) -> Interaction {
    let mut options = vec![option("name", CommandOptionValue::String(name.to_owned()))];
    if let Some(view) = view {
        options.push(option("view", CommandOptionValue::String(view.to_owned())));
    }
    let mut input = from_member(
        slash("command-inspect", Some(CHANNEL), options),
        88,
        Permissions::MANAGE_GUILD,
    );
    input.id = Id::new(id);
    input.token = format!("inspect-eligibility-{id}");
    input
}

async fn dispatch(
    runtime: &Arc<CommandRuntime>,
    mock: &MockRest,
    input: Interaction,
) -> Vec<RestRequest> {
    let start = mock.requests().len();
    assert!(
        runtime.dispatch(&interaction_event(input)),
        "actual gateway must own the interaction"
    );
    wait_for(
        || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
        "inspect gateway work settles",
    )
    .await;
    mock.requests()[start..].to_vec()
}

fn no_mentions(body: &Value) {
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(
            body["allowed_mentions"][key].is_null() || body["allowed_mentions"][key] == json!([])
        );
    }
    assert_ne!(body["allowed_mentions"]["replied_user"], true);
}

fn private_reply(requests: &[RestRequest]) -> String {
    assert!(!requests.is_empty(), "inspect cannot disappear into a stub");
    assert!(
        requests.iter().all(|request| {
            (request.method == "POST" && request.path.ends_with("/callback"))
                || (request.method == "PATCH" && request.path.ends_with("/messages/@original"))
        }),
        "no execution, followup, registry or other HTTP traffic: {requests:?}"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
    let ack: Value = serde_json::from_slice(&requests[0].body).expect("callback JSON");
    assert_eq!(
        ack["data"]["flags"].as_u64().unwrap_or(0) & 64,
        64,
        "private acknowledgement"
    );
    no_mentions(&ack["data"]);
    let body = if ack["type"] == 5 {
        assert_eq!(requests.len(), 2, "one defer and one private completion");
        assert_eq!(requests[1].method, "PATCH");
        let body: Value = serde_json::from_slice(&requests[1].body).expect("completion JSON");
        no_mentions(&body);
        body
    } else {
        assert_eq!(ack["type"], 4);
        assert_eq!(requests.len(), 1);
        ack["data"].clone()
    };
    assert!(body["embeds"].is_null() || body["embeds"] == json!([]));
    assert!(body["attachments"].is_null() || body["attachments"] == json!([]));
    let content = body["content"].as_str().expect("plain private content");
    assert!(!content.is_empty());
    assert!(
        content.encode_utf16().count() <= 2000,
        "bounded after escaping, headings and eligibility"
    );
    content.to_owned()
}

async fn snapshot(pool: &PgPool) -> (Vec<Value>, Vec<Value>) {
    let rows =
        sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c ORDER BY guild_id, name")
            .fetch_all(pool)
            .await
            .expect("every saved field, including timestamps");
    let audit = sqlx::query_scalar("SELECT to_jsonb(a) FROM automation_audit_log a ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("all audits, not just successful runs");
    (rows, audit)
}

async fn seed(pool: &PgPool, name: &str, trigger: Option<&str>, enabled: bool) {
    custom_command_store::put_command(
        pool,
        &StoredCommand {
            guild_id: GUILD_S.to_owned(),
            name: name.to_owned(),
            description: "Saved response".to_owned(),
            template: "PRIVATE TEMPLATE {username}".to_owned(),
            text_trigger: trigger.map(str::to_owned),
            enabled,
        },
        "66",
        "67",
        "2026-10-01T00:00:00Z",
        "2026-10-02T00:00:00Z",
    )
    .await
    .expect("stored fixture, including imported reserved triggers");
}

fn assert_saved_and_eligibility(content: &str, enabled: bool) -> String {
    let lower = content.to_lowercase();
    let saved = lower.lines().find(|line| {
        line.contains("saved")
            && (line.contains("enabled") || line.contains("disabled") || line.contains("paused"))
            && !line.contains("template")
            && !line.contains("eligib")
    });
    assert!(
        saved.is_some(),
        "separate saved-definition state is required: {content}"
    );
    let saved = saved.expect("saved-state line was asserted present");
    assert!(
        saved.contains(if enabled { "enabled" } else { "disabled" })
            || (!enabled && saved.contains("paused")),
        "saved state is not the effective dispatch switch: {content}"
    );
    assert!(
        !lower.contains("private template"),
        "summary must not run or reveal the template body"
    );
    for claim in [
        "execution: guaranteed",
        "delivery: guaranteed",
        "registration: synchronized",
        "registration: synced",
    ] {
        assert!(
            !lower.contains(claim),
            "eligibility is not delivery or Discord registry evidence: {content}"
        );
    }
    let eligibility = lower
        .lines()
        .find(|line| line.contains("text") && line.contains("eligib"));
    assert!(
        eligibility.is_some(),
        "separate current text-trigger eligibility is required: {content}"
    );
    eligibility
        .expect("eligibility line was asserted present")
        .to_owned()
}

fn assert_eligible(content: &str, eligibility: &str) {
    assert!(
        eligibility.contains("eligible")
            && !eligibility.contains("ineligible")
            && !eligibility.contains("not eligible")
            && !eligibility.contains("unknown")
            && !eligibility.contains("unavailable"),
        "passing configuration must be accurately qualified: {content}"
    );
    let lower = content.to_lowercase();
    assert!(
        lower.contains("automod")
            && (lower.contains("permission") || lower.contains("authorization")),
        "eligibility remains subject to normal per-message authorization and automod: {content}"
    );
}

#[test]
fn inspect_eligibility_prefix_reservation_control_uses_dispatch_not_write_reservations() {
    let dispatch_names = builtin_command_names();
    let write_names = reserved_command_names();
    for name in ["setup", "templateassistant"] {
        assert!(write_names.contains(name));
        assert!(!dispatch_names.contains(name));
    }
    // Literal outcomes characterize the unchanged prefix contract independently
    // of the new summary. Voice publication is not prefix-trigger reservation.
    for (automations, text, content, want) in [
        (true, true, "!faq one two", Some("!faq")),
        (true, true, "!help", None),
        (true, true, "!kick", None),
        (true, true, "!setup", Some("!setup")),
        (true, true, "!templateassistant", Some("!templateassistant")),
        (true, false, "!faq", None),
        (false, true, "!faq", None),
        (false, false, "!faq", None),
    ] {
        assert_eq!(
            accepted_text_trigger(automations, text, false, content, &dispatch_names).as_deref(),
            want,
            "unchanged prefix admission for {content}"
        );
    }
}

#[tokio::test]
async fn inspect_eligibility_effective_flag_row_and_voice_reservation_matrix_is_read_only() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable bootstrap");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded migrated test DB");
    // The slash name stays inspectable even if its imported trigger is a builtin.
    let cases = [
        ("slashonly", None, false),
        ("faq", Some("!faq"), false),
        ("reserved", Some("!help"), true),
        ("legacysetup", Some("!setup"), false),
        ("legacyassistant", Some("!templateassistant"), false),
    ];
    for (name, trigger, _) in cases {
        seed(db.pool(), name, trigger, true).await;
    }
    let mut observations = Vec::new();
    let mut snapshots = Vec::new();
    let mut id = 9200;
    for enabled in [true, false] {
        sqlx::query("UPDATE automation_commands SET enabled = $1")
            .bind(enabled)
            .execute(db.pool())
            .await
            .expect("fixture saved state");
        for (automations, text) in [(true, true), (true, false), (false, true), (false, false)] {
            for (voice, assistant) in [(false, false), (true, false), (false, true), (true, true)] {
                let flags = Flags {
                    automations,
                    text,
                    voice,
                    assistant,
                };
                let before = snapshot(db.pool()).await;
                let (mock, origin) = MockRest::start(vec![]).await;
                let runtime = configured_runtime(db.pool().clone(), origin, flags);
                let guard = runtime.dispatch_guard();
                for (name, trigger, reserved) in cases {
                    id += 1;
                    let requests = dispatch(&runtime, &mock, invocation(id, name, None)).await;
                    observations.push((flags, enabled, name, trigger, reserved, requests));
                }
                let after = snapshot(db.pool()).await;
                snapshots.push((before, after, runtime.published_commands().is_none()));
                drop(guard);
                drop(runtime);
                mock.shutdown().await;
            }
        }
    }
    db.close()
        .await
        .expect("teardown before behavioral RED assertions");
    assert_eq!(
        observations.len(),
        160,
        "complete raw flag/row/voice matrix"
    );
    for (before, after, unpublished) in snapshots {
        assert_eq!(
            before, after,
            "inspect cannot change rows, timestamps, audits or durable attempts"
        );
        assert!(unpublished, "pure inspection cannot publish the registry");
    }
    for (flags, enabled, name, trigger, reserved, requests) in observations {
        let content = private_reply(&requests);
        if !flags.automations {
            assert!(
                content.contains(AUTOMATIONS_DISABLED_REPLY),
                "preserve the existing management gate: {content}"
            );
            assert!(!content.starts_with("Summary /"));
            continue;
        }
        assert!(content.starts_with(&format!("Summary /{name}\n")));
        let eligibility = assert_saved_and_eligibility(&content, enabled);
        if enabled && flags.text && trigger.is_some() && !reserved {
            assert_eligible(&content, &eligibility);
        } else {
            assert!(eligibility.contains("ineligible") || eligibility.contains("not eligible"),
                "one failing prerequisite cannot become eligible: {flags:?} {name} enabled={enabled}: {content}");
            let lower = content.to_lowercase();
            // Multiple simultaneous blockers may be listed or prioritized. A
            // single-blocker configuration must name that exact cause/action.
            let reasons = [
                (
                    trigger.is_none(),
                    eligibility.contains("no text trigger") || eligibility.contains("no trigger"),
                    content.contains("/command name:") && content.contains("text-trigger:"),
                ),
                (
                    !enabled,
                    eligibility.contains("paused") || eligibility.contains("disabled definition"),
                    content.contains("/command-enable"),
                ),
                (
                    !flags.text,
                    eligibility.contains("gate")
                        || eligibility.contains("text commands disabled")
                        || eligibility.contains("text commands are disabled"),
                    content.contains("TWO_TEXT_COMMANDS")
                        && (lower.contains("operator") || lower.contains("configuration")),
                ),
                (
                    reserved,
                    eligibility.contains("builtin")
                        || eligibility.contains("built-in")
                        || eligibility.contains("reserved"),
                    content.contains("/command name:") && content.contains("text-trigger:"),
                ),
            ];
            assert!(reasons.iter().any(|(applicable, explained, actionable)| *applicable && *explained && *actionable),
                "name a true blocking reason and a safe existing corrective action: {flags:?} {name} enabled={enabled}: {content}");
        }
        assert!(content.contains(&format!("/command-inspect name:{name} view:template")));
        assert!(content.contains(&format!("/command-inspect name:{name} view:preview")));
    }
}

#[tokio::test]
async fn inspect_eligibility_help_details_explain_saved_state_versus_conditional_text_eligibility()
{
    let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new().host("agent-testdb"));
    pool.close().await;
    let (mock, origin) = MockRest::start(vec![]).await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(true, false)),
        GUILD,
        true,
    );
    // Help describes the last confirmed registry, never an unpublished guess.
    // Publication is fixture setup through loopback, not an inspect side effect.
    runtime
        .publish_registry_checked(Some(1111))
        .await
        .expect("confirmed help fixture publication");
    let confirmed = runtime.published_commands().expect("confirmed snapshot");
    let guard = runtime.dispatch_guard();
    let mut input = from_member(
        slash(
            "help",
            Some(CHANNEL),
            vec![option(
                "command",
                CommandOptionValue::String("command-inspect".to_owned()),
            )],
        ),
        88,
        Permissions::MANAGE_GUILD,
    );
    input.id = Id::new(9500);
    let requests = dispatch(&runtime, &mock, input).await;
    assert!(Arc::ptr_eq(
        &confirmed,
        &runtime
            .published_commands()
            .expect("same confirmed snapshot")
    ));
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
    let content = private_reply(&requests).to_lowercase();
    assert!(
        content.contains("saved")
            && content.contains("eligib")
            && (content.contains("text") || content.contains("prefix")),
        "the existing help workflow must describe the new distinction: {content}"
    );
}

#[tokio::test]
async fn inspect_eligibility_production_bootstrap_threads_effective_gate_without_changing_other_views(
) {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable bootstrap");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded migrated test DB");
    seed(db.pool(), "faq", Some("!faq"), true).await;
    let before = snapshot(db.pool()).await;
    let mut observations = Vec::new();
    for (index, text) in [false, true].into_iter().enumerate() {
        let (mock, origin) = MockRest::start_script(vec![
            RestResponse {
                status: 200,
                body: Some(r#"{"id":"1111"}"#.to_owned()),
                delay: Duration::ZERO,
            },
            RestResponse {
                status: 200,
                body: Some(r#"{"id":"2222","name":"Test guild"}"#.to_owned()),
                delay: Duration::ZERO,
            },
        ])
        .await;
        let runtime = CommandRuntime::new(
            db.pool().clone(),
            executor_at(origin),
            router_with_commands(gates(true, false)),
            GUILD,
            true,
        );
        let guard = runtime.dispatch_guard();
        let vars = HashMap::from([
            ("TWO_AUTOMATIONS".to_owned(), "1".to_owned()),
            ("TWO_TEXT_COMMANDS".to_owned(), u8::from(text).to_string()),
            // Even with automod enabled, inspect is a read, not a synthetic
            // accepted message or a promise that later messages pass automod.
            ("TWO_AUTOMOD".to_owned(), "1".to_owned()),
        ]);
        runtime
            .initialize_custom_commands(
                crate::gateway_commands::GatewayCommandConfig::from_map_for_activation(
                    GUILD, &vars, false,
                )
                .expect("effective production config"),
            )
            .await
            .expect("real application/guild bootstrap through loopback");
        let mut views = Vec::new();
        for (offset, view) in ["summary", "template", "preview"].into_iter().enumerate() {
            views.push(
                dispatch(
                    &runtime,
                    &mock,
                    invocation(9600 + (index * 3 + offset) as u64, "faq", Some(view)),
                )
                .await,
            );
        }
        let unpublished = runtime.published_commands().is_none();
        observations.push((text, views, unpublished));
        drop(guard);
        drop(runtime);
        mock.shutdown().await;
    }
    let after = snapshot(db.pool()).await;
    db.close()
        .await
        .expect("fixture teardown before behavioral assertions");
    assert_eq!(
        before, after,
        "all three views stay read-only under both text gates"
    );
    let mut summaries = Vec::new();
    for (text, views, unpublished) in observations {
        assert!(unpublished, "bootstrap/inspect do not publish commands");
        let summary = private_reply(&views[0]);
        let eligibility = assert_saved_and_eligibility(&summary, true);
        if text {
            assert_eligible(&summary, &eligibility);
        } else {
            assert!(eligibility.contains("ineligible") || eligibility.contains("not eligible"));
            assert!(eligibility.contains("gate") || eligibility.contains("disabled"));
            assert!(summary.contains("TWO_TEXT_COMMANDS"));
        }
        assert_eq!(
            private_reply(&views[1]),
            "Saved template /faq\nPRIVATE TEMPLATE {username}"
        );
        assert_eq!(
            private_reply(&views[2]),
            "Preview /faq\nPRIVATE TEMPLATE member"
        );
        summaries.push(summary);
    }
    assert_ne!(summaries[0], summaries[1], "the same enabled saved row must report the current effective text gate, not a cached/default status");
}
