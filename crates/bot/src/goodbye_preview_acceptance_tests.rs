#![cfg(test)]
//! Protected goodbye-preview behavior. No implementation diff was consulted.
//! Only loopback Discord and a disposable TestDatabase are used. The fixed
//! synthetic name deliberately exercises the LIVE renderer's safety boundary:
//! `Preview *member* @everyone\n<@765432109876543210>`, with a seven-day stay.
//! Builders preserve this file; do not add a real member option or lookup.

use super::*;
use crate::hotpath_settings_test_support::queries::{Queries, Query};
use std::panic::AssertUnwindSafe;
use two_bot_core::onboarding_store::record_prompted;
use two_bot_testsupport::TestDatabase;

const FIXTURE_ID: &str = "765432109876543210";
const FIXTURE_NAME: &str = "Preview *member* @everyone\n<@765432109876543210>";
const SAFE_NAME: &str = "Preview \\*member\\* @\u{200b}everyone <\u{200b}@765432109876543210>";
const DAYS: Option<i64> = Some(7);

fn packet(permissions: Option<&str>) -> Value {
    let mut member = json!({
        "user":{"id":"44","username":"invoker-not-the-fixture","discriminator":"0","bot":false},
        "roles":[],"deaf":false,"mute":false,"flags":0
    });
    if let Some(bits) = permissions {
        member["permissions"] = json!(bits);
    }
    json!({"op":0,"s":2,"t":"INTERACTION_CREATE","d":{
        "application_id":"1111","authorizing_integration_owners":{"0":"22"},
        "id":"333","token":"goodbye-preview-fixture","type":2,"version":1,
        "guild_id":"22","member":member,
        "data":{"id":"444","name":"goodbye-preview","type":1,"options":[]}
    }})
}

fn decode(packet: Value) -> Event {
    let parsed =
        twilight_gateway::parse(packet.to_string(), twilight_gateway::EventTypeFlags::all())
            .expect("valid gateway packet")
            .expect("interaction dispatch selected");
    Event::from(parsed)
}

fn invocation(event: &Event) -> &Interaction {
    let Event::InteractionCreate(interaction) = event else {
        panic!("interaction fixture")
    };
    &interaction.0
}

async fn preview_discord() -> MockRest {
    MockRest::with_responder(|request: &RestRequest| {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/api/v10/interactions/333/goodbye-preview-fixture/callback") => {
                ScriptedResponse::status(204)
            }
            ("PATCH", "/api/v10/webhooks/1111/goodbye-preview-fixture/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            // Record forbidden calls as well as rejecting them. The final
            // exact request census catches even a swallowed transport error.
            _ => ScriptedResponse::status(403),
        }
    })
    .await
}

fn assert_private_completion(mock: &MockRest) -> Value {
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        2,
        "only defer + edit; no channel post/member read/role write: {:?}",
        requests
            .iter()
            .map(|r| (&r.method, &r.path))
            .collect::<Vec<_>>()
    );
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/callback"));
    let defer: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(defer["type"], 5);
    assert_eq!(defer["data"]["flags"], 64, "ephemeral from the first ACK");
    assert_eq!(requests[1].method, "PATCH");
    assert!(requests[1].path.ends_with("/messages/@original"));
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    for key in ["parse", "roles", "users"] {
        assert_eq!(
            body["allowed_mentions"][key],
            json!([]),
            "no {key} mentions"
        );
    }
    assert!(
        body.get("components")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
        "no live controls"
    );
    let text = body["content"].as_str().expect("private text");
    assert!(
        two_bot_core::message_safety::text_len(text) <= two_bot_core::message_safety::CONTENT_LIMIT
    );
    assert!(!two_bot_core::message_safety::contains_mass_mention(text));
    body
}

fn assert_banner(text: &str) {
    let first = text.lines().next().unwrap_or_default().to_lowercase();
    assert!(
        first.contains("preview") && first.contains("not a delivery"),
        "clear non-delivery banner: {text}"
    );
}

fn expected_goodbye() -> String {
    // Independent literal proves sanitization rather than comparing the same
    // helper to itself. The second assertion pins parity with the live domain.
    let literal = format!("**{SAFE_NAME}** left the server (was here 7 days). Their messages and voice history stay on the books.");
    let Some(GoodbyeEffect::Post {
        channel_id,
        content,
        mentions,
    }) = adjudicate_goodbye(
        OnboardingMode::Session,
        FIXTURE_NAME,
        DAYS,
        Some("13"),
        false,
    )
    else {
        panic!("postable synthetic fixture")
    };
    assert_eq!(channel_id, "13");
    assert_eq!(mentions, MentionPolicy::None);
    assert_eq!(content, literal);
    literal
}

fn assert_render(text: &str, channel: &str) {
    assert_banner(text);
    assert!(
        text.contains(&format!("<#{channel}>")),
        "show configured destination: {text}"
    );
    let expected = expected_goodbye();
    assert!(
        text.ends_with(&expected),
        "preview body equals live adjudication, not a welcome or invoker: {text}"
    );
    assert!(!text.contains("invoker-not-the-fixture"));
    assert!(
        !text.contains(FIXTURE_NAME),
        "untrusted display name must be sanitized"
    );
}

fn assert_guidance(text: &str, reason: &[&str], next_action: &[&str]) {
    assert_banner(text);
    let lower = text.to_lowercase();
    assert!(
        reason.iter().any(|word| lower.contains(word)),
        "truthful reason {reason:?}: {text}"
    );
    assert!(
        next_action.iter().any(|word| lower.contains(word)),
        "actionable guidance {next_action:?}: {text}"
    );
    assert!(
        !text.contains("left the server"),
        "no fake render when not postable"
    );
    assert!(!text.contains("<\u{200b}@765432109876543210>"));
    for false_claim in ["successfully posted", "goodbye sent", "member removed"] {
        assert!(!lower.contains(false_claim));
    }
}

fn assert_no_member_or_delivery_sql(queries: &[Query], settings_expected: bool) {
    if settings_expected {
        assert!(
            queries
                .iter()
                .any(|q| q.normalized().contains("guild_settings")),
            "positive observer control: the preview must read current settings"
        );
    }
    for query in queries {
        let sql = query.normalized();
        assert!(
            !["insert ", "update ", "delete "]
                .iter()
                .any(|verb| sql.starts_with(verb)),
            "preview writes nothing: {sql}"
        );
        for forbidden in [
            " events",
            "member",
            "privacy_opt_out",
            "onboarding_prompt",
            "pg_advisory",
        ] {
            assert!(
                !sql.contains(forbidden),
                "no member/prompt/delivery access: {sql}"
            );
        }
    }
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("goodbye acceptance requires disposable TWO_TEST_DATABASE_URL");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

async fn seed_guards(pool: &PgPool) {
    for id in [FIXTURE_ID, "44", "55"] {
        assert!(
            record_prompted(pool, "22", id, "12", "2026-10-01T00:00:00+00:00")
                .await
                .unwrap()
        );
    }
}

async fn assert_guards_intact(pool: &PgPool) {
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 3,
        "zero funnel writes/deletes, including prompt-guard release"
    );
    for id in [FIXTURE_ID, "44", "55"] {
        assert!(
            has_onboarding_prompt(pool, "22", id).await.unwrap(),
            "existing guard retained: {id}"
        );
    }
}

async fn deliver_packet(runtime: &OnboardingRuntime, event: &Event) -> Vec<Query> {
    let pipeline = build_pipeline(Vec::new(), None);
    let captured = runtime
        .capture(event, &pipeline)
        .expect("goodbye-preview gateway packet is captured");
    let (result, queries) = Queries::capture(runtime.handle(captured, NOW)).await;
    result.expect("preview completes privately");
    queries
}

async fn db_case(mode: &str, dry_run: bool, channel: &str, check: impl FnOnce(&str)) {
    let db = database().await;
    let mock = preview_discord().await;
    let result = AssertUnwindSafe(async {
        seed_guards(db.pool()).await;
        let mut deployment = vars(mode, dry_run);
        deployment.insert("DISCORD_GOODBYE_CHANNEL_IDS".into(), channel.into());
        let runtime = runtime(db.pool(), &mock, &deployment);
        let queries = deliver_packet(&runtime, &decode(packet(Some("32")))).await;
        let body = assert_private_completion(&mock);
        assert_no_member_or_delivery_sql(&queries, true);
        assert_guards_intact(db.pool()).await;
        check(body["content"].as_str().unwrap());
    })
    .catch_unwind()
    .await;
    mock.shutdown().await;
    db.close().await.unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn goodbye_preview_adversarial_fixture_has_a_hand_derived_live_render() {
    let text = expected_goodbye();
    assert!(text.contains("\\*member\\*"));
    assert!(!text.contains('\n'));
    assert!(!text.contains("@everyone"));
    assert!(!text.contains("<@"));
}

#[tokio::test]
async fn goodbye_preview_gateway_packet_renders_sanitized_live_body_without_delivery_or_guard_release(
) {
    db_case("session", false, "13,14", |text| assert_render(text, "13")).await;
}

#[tokio::test]
async fn goodbye_preview_uses_current_stored_destination_without_member_reads_or_writes() {
    let db = database().await;
    let mock = preview_discord().await;
    let result = AssertUnwindSafe(async {
        seed_guards(db.pool()).await;
        let runtime = runtime(db.pool(), &mock, &vars("session", false));
        SettingsStore::new(db.pool())
            .set(
                "22",
                "DISCORD_GOODBYE_CHANNEL_IDS",
                Some(json!("14")),
                "fixture",
            )
            .await
            .unwrap();
        let queries = deliver_packet(&runtime, &decode(packet(Some("32")))).await;
        let body = assert_private_completion(&mock);
        assert_render(body["content"].as_str().unwrap(), "14");
        assert!(!body["content"].as_str().unwrap().contains("<#13>"));
        assert_no_member_or_delivery_sql(&queries, true);
        assert_guards_intact(db.pool()).await;
    })
    .catch_unwind()
    .await;
    mock.shutdown().await;
    db.close().await.unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn goodbye_preview_legacy_mode_gives_next_action_instead_of_fake_render() {
    db_case("legacy", false, "13", |text| {
        assert_guidance(
            text,
            &["session", "mode"],
            &["configure", "enable", "switch", "ask"],
        )
    })
    .await;
}

#[tokio::test]
async fn goodbye_preview_anchor_mode_gives_next_action_instead_of_fake_render() {
    db_case("anchor", false, "13", |text| {
        assert_guidance(
            text,
            &["session", "mode"],
            &["configure", "enable", "switch", "ask"],
        )
    })
    .await;
}

#[tokio::test]
async fn goodbye_preview_dry_run_gives_next_action_instead_of_fake_render() {
    db_case("session", true, "13", |text| {
        assert_guidance(
            text,
            &["dry run", "dry-run", "dry_run"],
            &["disable", "turn off", "configure", "ask"],
        )
    })
    .await;
}

#[tokio::test]
async fn goodbye_preview_missing_destination_gives_next_action_instead_of_fake_render() {
    db_case("session", false, "", |text| {
        assert_guidance(
            text,
            &["channel", "destination"],
            &["configure", "set", "ask"],
        )
    })
    .await;
}

async fn closed_pool() -> PgPool {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .unwrap();
    pool.close().await;
    pool
}

#[tokio::test]
async fn goodbye_preview_configuration_unavailable_finishes_private_defer_with_next_action() {
    let pool = closed_pool().await;
    let mock = preview_discord().await;
    let runtime = runtime(&pool, &mock, &vars("session", false));
    let event = decode(packet(Some("32")));
    let executor = ActionExecutor::with_proxy(
        "goodbye-preview-test-only-token".into(),
        Some(mock.origin()),
    )
    .unwrap();
    let gates = two_bot_core::RouterGates {
        configured_guild: Some(22),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: true,
        session_picker: true,
    };
    let shared = crate::command_runtime::CommandRuntime::new(
        pool.clone(),
        executor,
        crate::command_runtime::router_with_commands(gates),
        22,
        false,
    );
    let _dispatch_guard = shared.dispatch_guard();
    shared.on_interaction(invocation(&event)).await;
    assert!(
        mock.requests().is_empty(),
        "shared command consumer must yield ACK ownership to onboarding"
    );
    deliver_packet(&runtime, &event).await;
    let body = assert_private_completion(&mock);
    assert_guidance(
        body["content"].as_str().unwrap(),
        &["unavailable", "couldn't load", "could not load"],
        &["try again", "retry", "ask"],
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn goodbye_preview_refuses_missing_or_insufficient_permissions_before_any_settings_read() {
    let pool = closed_pool().await;
    for permissions in [None, Some("0"), Some("8"), Some("16")] {
        let mock = preview_discord().await;
        let runtime = runtime(&pool, &mock, &vars("session", false));
        let queries = deliver_packet(&runtime, &decode(packet(permissions))).await;
        assert!(queries.is_empty(), "permission denial must precede SQL");
        let body = assert_private_completion(&mock);
        assert_eq!(
            body["content"],
            two_bot_core::RouterRefusal::ManageServerRequired.message()
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn goodbye_preview_invalid_origin_controls_stay_silent() {
    let pool = closed_pool().await;
    let mock = preview_discord().await;
    let runtime = runtime(&pool, &mock, &vars("session", false));
    let pipeline = build_pipeline(Vec::new(), None);
    let mut foreign = packet(Some("32"));
    foreign["d"]["guild_id"] = json!("23");
    let mut bot = packet(Some("32"));
    bot["d"]["member"]["user"]["bot"] = json!(true);
    let mut memberless = packet(Some("32"));
    memberless["d"]["member"] = Value::Null;
    for packet in [foreign, bot, memberless] {
        let event = decode(packet);
        assert!(runtime.capture(&event, &pipeline).is_none());
        runtime
            .handle(
                OnboardingJob::Interaction(Box::new(invocation(&event).clone())),
                NOW,
            )
            .await
            .unwrap();
    }
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}
