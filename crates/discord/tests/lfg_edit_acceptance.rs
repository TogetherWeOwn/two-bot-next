//! Protected behavioral contract for metadata-only LFG edits.
//! Real router, store, migrations and REST executor; only Discord is loopback I/O.
//! Each database is disposable. No application database or live Discord is used.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    commands::PERM_MANAGE_EVENTS, lfg, lfg_store as store, RouterGates,
    ANNOUNCEMENTS_DISABLED_REPLY, MANAGE_EVENTS_REQUIRED,
};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const MESSAGE: &str = "5000";
const POST: &str = "lfg-edit-fixture";
const BOT: u64 = 1111;
const NEW_TIME: &str = "2099-09-12T19:15:00.000Z";

fn gates(enabled: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        announcements: enabled,
        scorecard: false,
        automations: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn invocation(id: u64, user: u64, permissions: u64, data: Value, kind: u8) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": BOT.to_string(), "type": kind,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"user": {"id": user.to_string(), "username": "member", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0, "permissions": permissions.to_string()},
        "token": format!("lfg-edit-fixture-{id}"), "version": 1, "entitlements": [],
        "authorizing_integration_owners": {}, "data": data
    }))
    .expect("wire interaction")
}

fn edit(id: u64, post: &str, title: Option<&str>, starts_at: Option<&str>) -> Interaction {
    let mut options = vec![json!({"type": 3, "name": "id", "value": post})];
    if let Some(title) = title {
        options.push(json!({"type": 3, "name": "title", "value": title}));
    }
    if let Some(starts_at) = starts_at {
        options.push(json!({"type": 3, "name": "starts-at", "value": starts_at}));
    }
    invocation(
        id,
        3333,
        PERM_MANAGE_EVENTS,
        json!({"id": "1", "name": "lfg-edit", "type": 1, "options": options}),
        2,
    )
}

fn select(id: u64, user: u64, role: &str) -> Interaction {
    invocation(
        id,
        user,
        0,
        json!({"custom_id": "two:lfg:lfg-edit-fixture", "component_type": 3, "values": [role]}),
        3,
    )
}

fn close(id: u64) -> Interaction {
    invocation(
        id,
        3333,
        PERM_MANAGE_EVENTS,
        json!({"id": "1", "name": "lfg-close", "type": 1,
            "options": [{"type": 3, "name": "id", "value": POST}]}),
        2,
    )
}

fn runtime(pool: sqlx::PgPool, mock: &MockRest, enabled: bool) -> InteractionRuntime {
    InteractionRuntime::new(
        gates(enabled),
        pool,
        ActionExecutor::with_proxy("lfg-edit-test-token".into(), Some(mock.origin()))
            .expect("loopback executor"),
        BOT,
        two_bot_core::ClassifierConfig::default(),
    )
}

async fn database() -> (tokio::sync::SemaphorePermit<'static>, TestDatabase) {
    // Also bound fixture creation in the normal parallel CI integration run.
    // The concurrency cases still use independent pools and runtime instances.
    static FIXTURES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let permit = FIXTURES.acquire().await.expect("fixture semaphore");
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    (permit, db)
}

async fn seed(pool: &sqlx::PgPool, id: &str, guild: &str) {
    let post = lfg::LfgPost {
        id: id.into(),
        guild_id: guild.into(),
        channel_id: CHANNEL.into(),
        message_id: Some(MESSAGE.into()),
        title: "Original raid".into(),
        starts_at: "2099-09-11T20:00:00.000Z".into(),
        status: lfg::LfgStatus::Open,
        created_by: "7777".into(),
        created_at: "2026-09-30T00:00:00.123456Z".into(),
        closed_at: None,
    };
    let roles = lfg::spec_roles(
        id,
        &lfg::parse_role_spec("tank:Main tank:1,healer:Healer:2,dps:Damage:4").expect("role spec"),
    );
    store::put_lfg(pool, &post, &roles, true)
        .await
        .expect("seed post");
    for (user, role, joined) in [
        ("3334", "tank", "2026-09-30T00:01:00.123456Z"),
        ("3335", "healer", "2026-09-30T00:02:00.234567Z"),
        ("3336", "dps", "2026-09-30T00:03:00.345678Z"),
    ] {
        assert_eq!(
            store::signup_lfg(pool, guild, id, role, user, joined)
                .await
                .expect("seed signup"),
            lfg::SignupOutcome::Joined
        );
    }
}

// Whole SQL rows preserve sub-millisecond signup identity. Tuple versions also
// catch deleting/recreating identical rows or writing supposedly unchanged rows.
async fn snapshot(pool: &sqlx::PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'domain', jsonb_build_object(
            'posts', (SELECT coalesce(jsonb_agg(to_jsonb(p) ORDER BY p.id), '[]'::jsonb) FROM lfg_posts p),
            'roles', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY r.lfg_id, r.position), '[]'::jsonb) FROM lfg_roles r),
            'signups', (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.lfg_id, s.user_id), '[]'::jsonb) FROM lfg_signups s)),
          'versions', jsonb_build_object(
            'posts', (SELECT jsonb_agg(jsonb_build_array(p.ctid::text, p.xmin::text) ORDER BY p.id) FROM lfg_posts p),
            'roles', (SELECT jsonb_agg(jsonb_build_array(r.ctid::text, r.xmin::text) ORDER BY r.lfg_id, r.position) FROM lfg_roles r),
            'signups', (SELECT jsonb_agg(jsonb_build_array(s.ctid::text, s.xmin::text) ORDER BY s.lfg_id, s.user_id) FROM lfg_signups s)),
          'audits', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.id), '[]'::jsonb) FROM announcements_audit_log a))",
    )
    .fetch_one(pool)
    .await
    .expect("durable snapshot")
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("REST JSON body")
}

fn reply(requests: &[RestRequest]) -> Option<String> {
    requests.iter().rev().find_map(|request| {
        let payload = body(request);
        if request.path.contains("/webhooks/") {
            payload["content"].as_str().map(str::to_owned)
        } else if request.path.ends_with("/callback") {
            payload["data"]["content"].as_str().map(str::to_owned)
        } else {
            None
        }
    })
}

fn channel_requests(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|r| r.path.starts_with("/api/v10/channels/"))
        .collect()
}

fn assert_safe(requests: &[RestRequest]) {
    for request in requests {
        let payload = body(request);
        let data = if request.path.ends_with("/callback") {
            &payload["data"]
        } else {
            &payload
        };
        assert_eq!(data["allowed_mentions"]["parse"], json!([]));
        if let Some(content) = data["content"].as_str() {
            assert!(content.encode_utf16().count() <= 2000);
            assert!(!content.contains("@everyone") && !content.contains("@here"));
        }
    }
}

fn assert_metadata_only(before: &Value, after: &Value, title: &str, starts_at: &str) {
    let mut expected = before["domain"].clone();
    expected["posts"][0]["title"] = json!(title);
    expected["posts"][0]["starts_at"] = json!(starts_at);
    assert_eq!(after["domain"], expected, "edit changed non-metadata state");
    for table in ["roles", "signups"] {
        assert_eq!(
            after["versions"][table], before["versions"][table],
            "edit rewrote {table}"
        );
    }
}

fn assert_edit_handled(handled: bool) {
    assert!(
        handled,
        "the real router must recognize /lfg-edit, not ignore the command"
    );
}

fn assert_updated_time(text: &str) {
    // Independent oracle: 2099-09-12 21:15 +02:00 = 19:15 UTC = 4092923700.
    assert!(
        text.contains(NEW_TIME)
            || text.contains("<t:4092923700:F>")
            || text.contains("<t:4092923700:f>"),
        "reply must show the canonical updated instant: {text}"
    );
}

#[test]
fn registry_exposes_only_metadata_options_with_the_existing_management_permission() {
    let commands = two_bot_core::announcement_commands();
    let command = commands
        .iter()
        .find(|c| c.name == "lfg-edit")
        .expect("register /lfg-edit");
    assert_eq!(
        command.default_member_permissions.as_deref(),
        Some("8589934592")
    );
    assert!(!command.dm_permission);
    assert_eq!(
        command.options.len(),
        3,
        "role/channel/roster edits are not part of this command"
    );
    let id = command
        .options
        .iter()
        .find(|o| o.name == "id")
        .expect("id option");
    let title = command
        .options
        .iter()
        .find(|o| o.name == "title")
        .expect("title option");
    let starts_at = command
        .options
        .iter()
        .find(|o| o.name == "starts-at")
        .expect("time option");
    assert_eq!(id.required, Some(true));
    assert_eq!(id.max_length, Some(128));
    assert_eq!(title.max_length, Some(100));
    assert_ne!(title.required, Some(true));
    assert_ne!(starts_at.required, Some(true));
    for option in &command.options {
        assert_eq!(option.kind, 3, "metadata and id options use string input");
    }
}

async fn successful_edit(
    title: Option<&str>,
    starts_at: Option<&str>,
    expected_title: &str,
    expected_time: &str,
) {
    let (_serial, db) = database().await;
    seed(db.pool(), POST, GUILD).await;
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": MESSAGE}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let handled = rt
        .handle(&edit(9101, POST, title, starts_at))
        .await
        .expect("edit transport");
    let fresh = db.independent_pool().await.expect("independent reader");
    let after = snapshot(&fresh).await;
    let stored = store::get_lfg(&fresh, GUILD, POST)
        .await
        .expect("read post")
        .expect("preserved post");
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    fresh.close().await;
    db.close().await.expect("fixture teardown");

    assert_edit_handled(handled);
    assert_metadata_only(&before, &after, expected_title, expected_time);
    let edits = channel_requests(&requests);
    assert_eq!(
        edits.len(),
        1,
        "edit must refresh, never recreate the message"
    );
    assert_eq!(edits[0].method, "PATCH");
    assert_eq!(edits[0].path, "/api/v10/channels/4444/messages/5000");
    let rendered = body(edits[0]);
    let content = rendered["content"].as_str().expect("rendered content");
    assert!(content.contains(expected_title));
    assert!(content.contains("**Main tank** 1/1") && content.contains("<@3334>"));
    assert!(content.contains("**Healer** 1/2") && content.contains("<@3335>"));
    assert!(content.contains("**Damage** 1/4") && content.contains("<@3336>"));
    assert_eq!(
        rendered["components"][0]["components"][0]["custom_id"],
        "two:lfg:lfg-edit-fixture"
    );
    let options = rendered["components"][0]["components"][0]["options"]
        .as_array()
        .expect("signup options");
    assert_eq!(
        options
            .iter()
            .map(|o| o["value"].as_str().expect("value"))
            .collect::<Vec<_>>(),
        ["tank", "healer", "dps", "__leave__"]
    );
    assert_eq!(
        requests.first().expect("initial acknowledgement").path,
        "/api/v10/interactions/9101/lfg-edit-fixture-9101/callback"
    );
    assert_eq!(body(&requests[0])["type"], 5);
    assert_eq!(body(&requests[0])["data"]["flags"], 64);
    let reply = reply(&requests).expect("private edit receipt");
    assert!(reply.to_lowercase().contains("updated") || reply.to_lowercase().contains("saved"));
    if starts_at.is_some() {
        assert_eq!(stored.starts_at, NEW_TIME);
        assert_updated_time(&reply);
        assert!(content.contains("Starts <t:4092923700:F>"));
    }
    let audits = after["audits"].as_array().expect("audit rows");
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0]["guild_id"], GUILD);
    assert_eq!(audits[0]["actor_id"], "3333");
    assert_eq!(audits[0]["target_key"], POST);
    assert_safe(&requests);
}

#[tokio::test]
async fn title_only_edit_preserves_all_ids_roles_capacities_and_signup_bytes() {
    successful_edit(
        Some("  Renamed raid  "),
        None,
        "Renamed raid",
        "2099-09-11T20:00:00+00:00",
    )
    .await;
}

#[tokio::test]
async fn time_only_edit_normalizes_the_offset_and_keeps_the_title_and_signups() {
    successful_edit(
        None,
        Some("2099-09-12T21:15:00+02:00"),
        "Original raid",
        "2099-09-12T19:15:00+00:00",
    )
    .await;
}

#[tokio::test]
async fn both_fields_edit_refreshes_the_same_post_with_the_new_metadata() {
    successful_edit(
        Some("Renamed raid"),
        Some("2099-09-12T21:15:00+02:00"),
        "Renamed raid",
        "2099-09-12T19:15:00+00:00",
    )
    .await;
}

#[tokio::test]
async fn exact_one_hundred_utf16_unit_title_is_accepted() {
    let title = "😀".repeat(50);
    successful_edit(Some(&title), None, &title, "2099-09-11T20:00:00+00:00").await;
}

#[tokio::test]
async fn multibyte_title_is_measured_in_utf16_not_utf8_bytes() {
    let title = "é".repeat(100);
    successful_edit(Some(&title), None, &title, "2099-09-11T20:00:00+00:00").await;
}

async fn refused_edit(
    post: &str,
    title: Option<&str>,
    starts_at: Option<&str>,
    expected_words: &[&str],
    closed: bool,
    guild: &str,
) {
    let (_serial, db) = database().await;
    seed(db.pool(), post, guild).await;
    if closed {
        assert!(
            store::close_lfg(db.pool(), guild, post, "2026-10-01T00:00:00Z")
                .await
                .expect("seed closed")
        );
    }
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": MESSAGE}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let handled = rt
        .handle(&edit(9201, post, title, starts_at))
        .await
        .expect("refusal transport");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("fixture teardown");

    assert_edit_handled(handled);
    assert_eq!(
        after, before,
        "refusal mutated domain rows, tuple versions or audit"
    );
    assert!(
        channel_requests(&requests).is_empty(),
        "refusal touched a channel"
    );
    let text = reply(&requests).expect("concise correction reply");
    let lower = text.to_lowercase();
    assert!(
        expected_words.iter().all(|word| lower.contains(word)),
        "missing correction {expected_words:?}: {text}"
    );
    assert!(
        text.encode_utf16().count() <= 300,
        "correction must be concise"
    );
    assert_safe(&requests);
}

#[tokio::test]
async fn at_least_one_metadata_field_is_required_without_a_write() {
    refused_edit(POST, None, None, &["title", "starts-at"], false, GUILD).await;
}

#[tokio::test]
async fn blank_title_refuses_without_saving_a_valid_time_either() {
    refused_edit(
        POST,
        Some("\u{feff}\t  "),
        Some(NEW_TIME),
        &["title", "1-100"],
        false,
        GUILD,
    )
    .await;
}

#[tokio::test]
async fn title_over_one_hundred_utf16_units_refuses_without_writes() {
    refused_edit(
        POST,
        Some(&"😀".repeat(51)),
        None,
        &["title", "1-100"],
        false,
        GUILD,
    )
    .await;
}

#[tokio::test]
async fn invalid_time_refuses_without_saving_a_valid_title_either() {
    refused_edit(
        POST,
        Some("Do not save me"),
        Some("tomorrow @everyone"),
        &["starts-at", "iso-8601"],
        false,
        GUILD,
    )
    .await;
}

#[tokio::test]
async fn past_time_refuses_without_writes() {
    refused_edit(
        POST,
        Some("Do not save me"),
        Some("2000-01-01T00:00:00Z"),
        &["starts-at", "future"],
        false,
        GUILD,
    )
    .await;
}

#[tokio::test]
async fn empty_time_is_not_an_omitted_optional_field() {
    refused_edit(
        POST,
        Some("Do not save me"),
        Some(""),
        &["starts-at", "iso-8601"],
        false,
        GUILD,
    )
    .await;
}

#[tokio::test]
async fn empty_post_id_refuses_even_when_a_matching_row_exists() {
    refused_edit(
        "",
        Some("Do not save me"),
        None,
        &["id", "128"],
        false,
        GUILD,
    )
    .await;
}

#[tokio::test]
async fn overlong_post_id_refuses_even_when_a_matching_row_exists() {
    refused_edit(
        &"😀".repeat(65),
        Some("Do not save me"),
        None,
        &["id", "128"],
        false,
        GUILD,
    )
    .await;
}

#[tokio::test]
async fn foreign_guild_target_has_no_domain_audit_or_discord_effect() {
    refused_edit(
        POST,
        Some("Do not save me"),
        None,
        &["missing"],
        false,
        "9999",
    )
    .await;
}

#[tokio::test]
async fn closed_post_cannot_be_edited_or_reopened() {
    refused_edit(
        POST,
        Some("Do not save me"),
        Some(NEW_TIME),
        &["closed"],
        true,
        GUILD,
    )
    .await;
}

#[tokio::test]
async fn missing_post_is_not_created_by_edit() {
    let (_serial, db) = database().await;
    seed(db.pool(), POST, GUILD).await;
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": MESSAGE}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let handled = rt
        .handle(&edit(
            9301,
            "missing-post",
            Some("No creation"),
            Some(NEW_TIME),
        ))
        .await
        .expect("transport");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("fixture teardown");
    assert_edit_handled(handled);
    assert_eq!(after, before);
    assert!(channel_requests(&requests).is_empty());
    assert!(reply(&requests)
        .expect("missing reply")
        .to_lowercase()
        .contains("missing"));
    assert_safe(&requests);
}

#[tokio::test]
async fn metadata_edit_requires_manage_events_and_honors_the_feature_gate() {
    let (_serial, db) = database().await;
    seed(db.pool(), POST, GUILD).await;
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": MESSAGE}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let unauthorized = invocation(
        9401,
        3333,
        0,
        json!({"id": "1", "name": "lfg-edit", "type": 1,
        "options": [{"type": 3, "name": "id", "value": POST}, {"type": 3, "name": "title", "value": "No permission"}]}),
        2,
    );
    let denied = rt
        .handle(&unauthorized)
        .await
        .expect("permission transport");
    let denied_requests = mock.requests();
    let off = runtime(db.pool().clone(), &mock, false);
    let disabled = off
        .handle(&edit(9402, POST, Some("Disabled"), None))
        .await
        .expect("feature transport");
    let requests = mock.requests();
    let after = snapshot(db.pool()).await;
    mock.shutdown().await;
    drop((rt, off));
    db.close().await.expect("fixture teardown");
    assert_edit_handled(denied);
    assert_edit_handled(disabled);
    assert_eq!(
        reply(&denied_requests).as_deref(),
        Some(MANAGE_EVENTS_REQUIRED)
    );
    assert_eq!(
        reply(&requests[denied_requests.len()..]).as_deref(),
        Some(ANNOUNCEMENTS_DISABLED_REPLY)
    );
    assert_eq!(after, before);
    assert!(channel_requests(&requests).is_empty());
    assert!(requests.iter().all(|r| r.path.ends_with("/callback")));
    assert_safe(&requests);
}

#[tokio::test]
async fn foreign_interaction_guild_is_ignored_before_callback_or_mutation_control() {
    let (_serial, db) = database().await;
    seed(db.pool(), POST, GUILD).await;
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let mut foreign = edit(9501, POST, Some("Foreign interaction"), None);
    foreign.guild_id = Some(twilight_model::id::Id::new(9999));
    let handled = rt.handle(&foreign).await.expect("foreign transport");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("fixture teardown");
    assert!(!handled);
    assert_eq!(after, before);
    assert!(requests.is_empty());
}

#[tokio::test]
async fn normalized_identical_fields_report_unchanged_without_rewriting_or_refreshing() {
    let (_serial, db) = database().await;
    seed(db.pool(), POST, GUILD).await;
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": MESSAGE}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let handled = rt
        .handle(&edit(
            9601,
            POST,
            Some("  Original raid\u{feff}"),
            Some("2099-09-11T22:00:00+02:00"),
        ))
        .await
        .expect("transport");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("fixture teardown");
    assert_edit_handled(handled);
    assert_eq!(after["domain"], before["domain"]);
    assert_eq!(
        after["versions"], before["versions"],
        "no-op must not rewrite rows"
    );
    let text = reply(&requests).expect("no-op reply").to_lowercase();
    assert!(
        text.contains("unchanged") || text.contains("no changes"),
        "no-op claimed an update: {text}"
    );
    assert!(channel_requests(&requests).is_empty());
    assert_safe(&requests);
}

#[tokio::test]
async fn hostile_title_is_stored_verbatim_but_the_existing_message_and_reply_cannot_ping() {
    let (_serial, db) = database().await;
    seed(db.pool(), POST, GUILD).await;
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": MESSAGE}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let hostile = "@everyone @here <@&7654321> **raid** https://example.test";
    let handled = rt
        .handle(&edit(9701, POST, Some(hostile), None))
        .await
        .expect("transport");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("fixture teardown");
    assert_edit_handled(handled);
    assert_metadata_only(&before, &after, hostile, "2099-09-11T20:00:00+00:00");
    assert_eq!(channel_requests(&requests).len(), 1);
    assert_safe(&requests);
}

#[tokio::test]
async fn database_refusal_rolls_back_both_metadata_fields_and_never_refreshes() {
    let (_serial, db) = database().await;
    seed(db.pool(), POST, GUILD).await;
    sqlx::query("ALTER TABLE lfg_posts ADD CONSTRAINT edit_time_fault CHECK (starts_at < '2099-09-12T00:00:00Z'::timestamptz)")
        .execute(db.pool()).await.expect("test-owned post-update fault");
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": MESSAGE}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let handled = rt
        .handle(&edit(9801, POST, Some("Must roll back"), Some(NEW_TIME)))
        .await
        .expect("transport");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("fixture teardown");
    assert_edit_handled(handled);
    assert_eq!(
        after, before,
        "one metadata field committed despite the other's failure"
    );
    assert!(channel_requests(&requests).is_empty());
    let text = reply(&requests).expect("failure reply").to_lowercase();
    assert!(text.contains("failed") || text.contains("could not"));
    assert!(!text.contains("edit_time_fault"), "database detail leaked");
    assert_safe(&requests);
}

async fn saved_edit_failure(audit_failure: bool) {
    let (_serial, db) = database().await;
    seed(db.pool(), POST, GUILD).await;
    if audit_failure {
        // The fault is after the independent metadata commit, not in its transaction.
        sqlx::query(
            "ALTER TABLE announcements_audit_log ADD CONSTRAINT edit_audit_fault CHECK (false)",
        )
        .execute(db.pool())
        .await
        .expect("test-owned audit fault");
    }
    let before = snapshot(db.pool()).await;
    let script = if audit_failure {
        vec![]
    } else {
        vec![ScriptedResponse::status(204), ScriptedResponse::status(500)]
    };
    let mock = MockRest::start(script, ScriptedResponse::json(200, json!({"id": MESSAGE}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let handled = rt
        .handle(&edit(9901, POST, Some("Saved rename"), Some(NEW_TIME)))
        .await
        .expect("transport");
    let fresh = db.independent_pool().await.expect("independent reader");
    let after = snapshot(&fresh).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    fresh.close().await;
    db.close().await.expect("fixture teardown");
    assert_edit_handled(handled);
    assert_metadata_only(&before, &after, "Saved rename", "2099-09-12T19:15:00+00:00");
    let text = reply(&requests).expect("honest degradation reply");
    let lower = text.to_lowercase();
    assert!(
        lower.contains("saved"),
        "reply must explicitly say the edit was saved: {text}"
    );
    assert!(
        lower.contains("refresh")
            && ["failed", "could not", "couldn't", "not refreshed"]
                .iter()
                .any(|s| lower.contains(s)),
        "reply must say the visible post was not refreshed: {text}"
    );
    assert_updated_time(&text);
    assert!(!text.contains("edit_audit_fault"));
    let edits = channel_requests(&requests);
    assert!(edits
        .iter()
        .all(|r| r.method == "PATCH" && r.path == "/api/v10/channels/4444/messages/5000"));
    if audit_failure {
        assert!(lower.contains("audit"), "audit loss must be disclosed");
        assert!(
            edits.is_empty(),
            "preserve existing no-refresh-after-audit-failure semantics"
        );
        assert_eq!(after["audits"], json!([]));
    } else {
        assert_eq!(edits.len(), 1, "one failed PATCH, no replacement POST");
        assert_eq!(after["audits"].as_array().expect("audits").len(), 1);
    }
    assert_safe(&requests);
}

#[tokio::test]
async fn saved_edit_with_failed_audit_discloses_saved_metadata_audit_loss_and_stale_visible_post() {
    saved_edit_failure(true).await;
}

#[tokio::test]
async fn saved_edit_with_failed_discord_refresh_discloses_saved_metadata_and_stale_visible_post() {
    saved_edit_failure(false).await;
}

// Observe a real cross-pool lock waiter, not scheduling order or a local permit.
async fn post_lock_waiter(pool: &sqlx::PgPool) -> bool {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
                 WHERE a.datname = current_database() AND l.locktype = 'advisory' AND NOT l.granted
                 AND cardinality(pg_blocking_pids(l.pid)) > 0)",
            )
            .fetch_one(pool)
            .await
            .expect("lock observation");
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[derive(Clone, Copy)]
enum Operation {
    Edit,
    Signup,
    Leave,
    Close,
}

impl Operation {
    fn interaction(self, id: u64) -> Interaction {
        match self {
            Self::Edit => edit(id, POST, Some("Serialized rename"), Some(NEW_TIME)),
            Self::Signup => select(id, 3337, "dps"),
            Self::Leave => select(id, 3334, "__leave__"),
            Self::Close => close(id),
        }
    }
}

async fn serialized_operations(first: Operation, second: Operation) {
    let (_serial, db) = database().await;
    seed(db.pool(), POST, GUILD).await;
    let initial = snapshot(db.pool()).await;
    let peer = db.independent_pool().await.expect("second runtime pool");
    let observer = db.independent_pool().await.expect("observer pool");
    // Callback succeeds, then the first operation's refresh remains unanswered.
    let (mock, gate) = MockRest::start_gated(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": MESSAGE})),
        ],
        ScriptedResponse::json(200, json!({"id": MESSAGE})),
    )
    .await;
    let first_rt = Arc::new(runtime(db.pool().clone(), &mock, true));
    let second_rt = Arc::new(runtime(peer.clone(), &mock, true));
    let first_worker = tokio::spawn({
        let rt = first_rt.clone();
        async move { rt.handle(&first.interaction(10001)).await }
    });
    let reached_refresh = tokio::time::timeout(Duration::from_secs(3), gate.wait_for_request())
        .await
        .is_ok();
    let before_contender = snapshot(&observer).await;
    let second_worker = tokio::spawn({
        let rt = second_rt.clone();
        async move { rt.handle(&second.interaction(10002)).await }
    });
    let waiting = post_lock_waiter(&observer).await;
    let during_contention = snapshot(&observer).await;
    let premature_channel_requests = channel_requests(&mock.requests()).len();
    // Always release the test-owned gate and join workers before RED assertions.
    gate.release();
    let first_result = tokio::time::timeout(Duration::from_secs(10), first_worker)
        .await
        .expect("first completed")
        .expect("first task")
        .expect("first transport");
    let second_result = tokio::time::timeout(Duration::from_secs(10), second_worker)
        .await
        .expect("second completed")
        .expect("second task")
        .expect("second transport");
    let after = snapshot(&observer).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop((first_rt, second_rt));
    peer.close().await;
    observer.close().await;
    db.close().await.expect("fixture teardown");

    assert!(
        reached_refresh,
        "first operation never reached the existing message refresh"
    );
    assert_edit_handled(first_result);
    assert_edit_handled(second_result);
    assert!(
        waiting,
        "independent runtime did not wait for the held per-post lock"
    );
    assert_eq!(
        during_contention, before_contender,
        "contender mutated before the first refresh completed"
    );
    assert_eq!(
        premature_channel_requests, 1,
        "contender refreshed out of order"
    );
    assert_eq!(after["domain"]["roles"], initial["domain"]["roles"]);
    assert_eq!(after["versions"]["roles"], initial["versions"]["roles"]);
    let post = &after["domain"]["posts"][0];
    let closed_first = matches!(first, Operation::Close);
    assert_eq!(
        post["title"],
        if closed_first {
            "Original raid"
        } else {
            "Serialized rename"
        }
    );
    assert_eq!(
        post["starts_at"],
        if closed_first {
            "2099-09-11T20:00:00+00:00"
        } else {
            "2099-09-12T19:15:00+00:00"
        }
    );
    let closed = matches!(first, Operation::Close) || matches!(second, Operation::Close);
    assert_eq!(post["status"], if closed { "closed" } else { "open" });
    assert_eq!(post["closed_at"].is_null(), !closed);
    for field in [
        "id",
        "guild_id",
        "channel_id",
        "message_id",
        "created_by",
        "created_at",
    ] {
        assert_eq!(
            post[field], initial["domain"]["posts"][0][field],
            "identity changed: {field}"
        );
    }
    let mut expected_signups = initial["domain"]["signups"]
        .as_array()
        .expect("initial signups")
        .clone();
    if matches!(first, Operation::Leave) {
        expected_signups.retain(|s| s["user_id"] != "3334");
    }
    let signups = after["domain"]["signups"]
        .as_array()
        .expect("final signups");
    for signup in &expected_signups {
        assert!(
            signups.contains(signup),
            "existing signup identity was lost or changed"
        );
    }
    if matches!(first, Operation::Signup) || matches!(second, Operation::Signup) {
        assert_eq!(signups.len(), 4);
        assert!(signups
            .iter()
            .any(|s| s["user_id"] == "3337" && s["role_key"] == "dps"));
    } else {
        assert_eq!(signups, &expected_signups);
    }
    let edits = channel_requests(&requests);
    assert_eq!(edits.len(), if closed_first { 1 } else { 2 });
    assert!(edits
        .iter()
        .all(|r| r.method == "PATCH" && r.path == "/api/v10/channels/4444/messages/5000"));
    let last = body(edits.last().expect("final visible state"));
    if closed {
        assert_eq!(last["components"], json!([]));
        assert!(last["content"]
            .as_str()
            .expect("content")
            .contains("Closed"));
    } else {
        let content = last["content"].as_str().expect("content");
        assert!(content.contains("Serialized rename") && content.contains("<t:4092923700:F>"));
        if matches!(first, Operation::Signup) || matches!(second, Operation::Signup) {
            assert!(
                content.contains("<@3337>"),
                "final refresh lost the simultaneous signup"
            );
        }
        if matches!(first, Operation::Leave) {
            assert!(
                !content.contains("<@3334>"),
                "edit resurrected a leaver visually"
            );
        }
    }
    assert_safe(&requests);
}

#[tokio::test]
async fn edit_serializes_with_a_simultaneous_signup_across_runtime_instances() {
    serialized_operations(Operation::Edit, Operation::Signup).await;
}

#[tokio::test]
async fn signup_serializes_with_a_simultaneous_edit_across_runtime_instances() {
    serialized_operations(Operation::Signup, Operation::Edit).await;
}

#[tokio::test]
async fn close_serializes_before_edit_and_the_edit_cannot_reopen_or_rename_it() {
    serialized_operations(Operation::Close, Operation::Edit).await;
}

#[tokio::test]
async fn edit_serializes_before_close_and_the_final_visible_post_stays_closed() {
    serialized_operations(Operation::Edit, Operation::Close).await;
}

#[tokio::test]
async fn leave_serializes_before_edit_and_the_edit_does_not_resurrect_the_signup() {
    serialized_operations(Operation::Leave, Operation::Edit).await;
}

#[tokio::test]
async fn existing_signup_and_close_control_use_the_same_real_router_and_database() {
    let (_serial, db) = database().await;
    seed(db.pool(), POST, GUILD).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": MESSAGE}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let signup = rt
        .handle(&select(10101, 3337, "dps"))
        .await
        .expect("signup transport");
    let closed = rt.handle(&close(10102)).await.expect("close transport");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("fixture teardown");
    assert!(signup && closed);
    assert_eq!(
        after["domain"]["signups"]
            .as_array()
            .expect("signups")
            .len(),
        4
    );
    assert_eq!(after["domain"]["posts"][0]["status"], "closed");
    assert_eq!(channel_requests(&requests).len(), 2);
    assert_safe(&requests);
}
