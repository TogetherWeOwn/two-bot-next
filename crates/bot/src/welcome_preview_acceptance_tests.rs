#![cfg(test)]
//! Acceptance: admins preview the current welcome without a real join.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the RED tests pass by changing
//! production code only (the onboarding slash capture/acknowledge/handle
//! path, the interaction-response adapter it needs, and the router,
//! registry, permission, help and reference updates enumerated in the
//! `test-handoff` document). Do not edit the fixtures or assertions: they
//! are the spec.
//!
//! The missing behavior: `accepts_interaction` takes GamePicker and
//! SessionPicker components only, so `/welcome-preview` (invocation shape
//! pinned in the core crate's frozen `welcome_preview_acceptance`: admin
//! Manage-Server gate, guild-only, zero options) is a silent no-op. The
//! fix routes that ONE slash through the EXISTING onboarding pipeline:
//!
//! - the shared capture admits it (configured guild, human member with any
//!   permission bits — the handler refuses non-admins privately after the
//!   defer; wrong guild, bots and memberless payloads stay silent);
//! - the existing receive-relative ephemeral acknowledge runs BEFORE any
//!   settings read, with no retry of an unconfirmed ack;
//! - the reply renders the CURRENT welcome through the LIVE renderer
//!   (`adjudicate_welcome` + `picker_components`) for the fixed fixture
//!   identity below, byte-identical to what a real join posts for the same
//!   config — message plus picker, plus a preview banner, as a private
//!   edit that pings nobody;
//! - the preview is read-only: zero role grants/removals, channel posts,
//!   funnel/prompt records, invites, voice moves or audit writes — and it
//!   resolves NO member state at all (neither the fixture's nor the
//!   invoker's);
//! - every reply except the non-admin refusal opens with the banner: first
//!   line, names `preview`, states `not a delivery`, names the fixture
//!   member id and the mode;
//! - disabled/unconfigured welcomes, substitution gaps (a slot the live
//!   renderer needs but config does not supply), malformed config and
//!   failed lookups get exactly one private guidance edit — never a menu,
//!   never a success claim, never a panic, never a silent drop.
//!
//! Contract note on "templates": the shipped renderer has fixed copy with
//! substitution slots (member mention, channel pills, occurrence time,
//! picker labels) — there is no configurable message template, and config
//! editing is out of scope. "Hostile template content" therefore means
//! hostile STORED CONFIG flowing toward a slot (it must fail the render
//! closed and never echo raw), and "substitution gaps" means slots whose
//! input config does not supply (each renders an explicit marker naming
//! the missing piece). Guild identity is boot-validated, so the
//! unconfigured case is empty channels / dry-run-off, not a missing guild.
//!
//! RED at baseline (12 tests): onboarding does not capture the slash; the
//! shared runtime competes for its ACK and admits it to the busy gate. Preview
//! rendering/click guidance is absent. Picker clicks must remain read-only
//! before config lookup while preserving the byte-identical live components.
//!
//! Controls (2 tests): wrong-guild/bot/memberless silence, and the
//! offline fixture + fail-closed-construction pins.
//!
//! This file brings its own strict mock Discord responder: after the defer
//! it answers ONLY the completing edit. Any settings-external read (member,
//! channel, guild, invites) panics the test — that panic IS the
//! no-member-resolution proof. It uses the sibling `TestSchema`, `runtime`,
//! `vars`, `welcome`, `discord`, `replies`, `posts` and
//! `has_onboarding_prompt` helpers verbatim; the builder must keep those
//! signatures stable.
//!
//! Disposable-DB only (plus offline tests): no live Discord, no
//! staging/production database, never `DATABASE_URL`.

use super::*;
use two_bot_discord::onboarding_messages::picker_components;

// --- fixed fixture identity -------------------------------------------------
// Documented synthetic newcomer. NOT a real member: no test resolves this
// id over Discord, and the preview must not either. Roles: none (a
// newcomer arrives roleless, so no game option preselects). Display name:
// never resolved and never rendered — the only fixture slot the live
// renderer substitutes is the member mention.

/// Synthetic newcomer id the preview renders for. Stable across runs.
const PREVIEW_FIXTURE_MEMBER_ID: u64 = 765_432_109_876_543_210;
/// Fixture mention pill, the single `<@` the live welcome bodies carry.
const PREVIEW_FIXTURE_PILL: &str = "<@765432109876543210>";

/// Non-admin refusal reuses the shared router sentence verbatim — no new
/// copy, no banner, no preview.
fn manage_server_refusal() -> String {
    two_bot_core::RouterRefusal::ManageServerRequired.message()
}

#[derive(Default)]
struct PreviewDiscordState {
    reject_callback: bool,
    close_pool_on_callback: Option<PgPool>,
}

/// Strict responder: the defer callback and the completing edit only. Every
/// other route — any member/channel/guild/invite read, any role write, any
/// channel post — panics, proving the preview performs none of them.
async fn preview_discord(state: Arc<Mutex<PreviewDiscordState>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let mut state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("POST", path) if path.ends_with("/callback") => {
                if state.reject_callback {
                    return ScriptedResponse::status(403);
                }
                if let Some(pool) = state.close_pool_on_callback.take() {
                    tokio::spawn(async move { pool.close().await });
                    return ScriptedResponse::status(204).delayed(Duration::from_millis(100));
                }
                ScriptedResponse::status(204)
            }
            ("PATCH", path) if path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            _ => panic!(
                "preview performs no Discord read or write besides defer+edit: {} {}",
                request.method, path
            ),
        }
    })
    .await
}

fn admin_member(user_id: &str, permissions: Option<&str>) -> Value {
    let mut member = json!({
        "roles": [], "deaf": false, "mute": false, "flags": 0,
        "user": {"id": user_id, "username": "admin", "discriminator": "0", "bot": false},
    });
    if let Some(bits) = permissions {
        member["permissions"] = Value::String(bits.to_owned());
    }
    member
}

fn bot_member() -> Value {
    json!({
        "permissions": "0", "roles": [], "deaf": false, "mute": false, "flags": 0,
        "user": {"id": "44", "username": "bot", "discriminator": "0", "bot": true},
    })
}

/// A `/welcome-preview` slash from admin member 44 in guild 22: the exact
/// Manage Server bit, zero options. `[]` is wire-identical to Discord
/// omitting the key, and sidesteps optional-field defaults.
fn preview() -> Interaction {
    preview_with_member(admin_member("44", Some("32")))
}

fn preview_with_member(member: Value) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": "22"},
        "id": "333", "token": "mock-callback", "type": 2, "version": 1, "guild_id": "22",
        "member": member,
        "data": {"id": "444", "name": "welcome-preview", "type": 1, "options": []},
    }))
    .expect("valid welcome-preview slash fixture")
}

fn preview_in_guild(guild_id: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": guild_id},
        "id": "333", "token": "mock-callback", "type": 2, "version": 1, "guild_id": guild_id,
        "member": admin_member("44", Some("32")),
        "data": {"id": "444", "name": "welcome-preview", "type": 1, "options": []},
    }))
    .expect("valid welcome-preview slash fixture")
}

fn preview_by_bot() -> Interaction {
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": "22"},
        "id": "333", "token": "mock-callback", "type": 2, "version": 1, "guild_id": "22",
        "member": bot_member(),
        "data": {"id": "444", "name": "welcome-preview", "type": 1, "options": []},
    }))
    .expect("valid welcome-preview slash fixture")
}

fn preview_without_member() -> Interaction {
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": "22"},
        "id": "333", "token": "mock-callback", "type": 2, "version": 1, "guild_id": "22",
        "member": Value::Null,
        "data": {"id": "444", "name": "welcome-preview", "type": 1, "options": []},
    }))
    .expect("valid welcome-preview slash fixture")
}

/// Click the actual menu returned by the preview. Keep the original message's
/// banner/components on the interaction: live and preview menus share IDs.
fn click_rendered_picker(body: &Value, mode: &str) -> Interaction {
    let menu = &body["components"][0]["components"][0];
    let custom_id = menu["custom_id"].as_str().expect("rendered picker id");
    let selection = if mode == "legacy" {
        "fallguys"
    } else {
        "find-players"
    };
    assert!(
        menu["options"]
            .as_array()
            .expect("rendered picker options")
            .iter()
            .any(|option| option["value"] == selection),
        "the click selects a valid, mutating live option"
    );
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": "22"},
        "id": "334", "token": "mock-picker-callback", "type": 3, "version": 1,
        "guild_id": "22", "channel_id": "12",
        "member": admin_member("44", Some("32")),
        "message": {
            "id": "98", "channel_id": "12", "type": 0,
            "author": {"id": "999", "username": "bot", "discriminator": "0", "bot": true},
            "content": body["content"], "components": body["components"], "flags": 64,
            "timestamp": "2026-10-01T00:00:00.000000+00:00",
            "attachments": [], "embeds": [], "mentions": [], "mention_roles": [],
            "mention_everyone": false, "pinned": false, "tts": false
        },
        "data": {"custom_id": custom_id, "component_type": 3, "values": [selection]}
    }))
    .expect("rendered-preview picker click fixture")
}

fn job(interaction: Interaction) -> OnboardingJob {
    OnboardingJob::Interaction(Box::new(interaction))
}

fn shared_preview_runtime(
    pool: &PgPool,
    mock: &MockRest,
) -> Arc<crate::command_runtime::CommandRuntime> {
    ensure_crypto_provider();
    let executor = ActionExecutor::with_proxy(
        "welcome-preview-test-only-token".into(),
        Some(mock.origin()),
    )
    .expect("loopback-only shared executor");
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
        session_picker: false,
    };
    crate::command_runtime::CommandRuntime::new(
        pool.clone(),
        executor,
        crate::command_runtime::router_with_commands(gates),
        22,
        false,
    )
}

fn now_secs() -> i64 {
    NOW / 1000
}

fn callbacks(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path.ends_with("/callback"))
        .collect()
}

fn role_writes(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| {
            (request.method == "PUT" || request.method == "DELETE")
                && request.path.contains("/roles/")
        })
        .collect()
}

fn single_edit_body(mock: &MockRest) -> Value {
    let edits = replies(mock);
    assert_eq!(
        edits.len(),
        1,
        "exactly one follow-up edit finishes the defer, got {}",
        edits.len()
    );
    serde_json::from_slice(&edits[0].body).unwrap()
}

fn edit_content(body: &Value) -> &str {
    body["content"].as_str().unwrap_or_default()
}

async fn event_count(db: &TestSchema) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

/// The defer-first half of every handled preview: the FIRST request is the
/// ephemeral defer, before any settings read.
fn assert_deferred_first(mock: &MockRest) {
    let requests = mock.requests();
    assert!(
        !requests.is_empty(),
        "the preview is captured and deferred before any read"
    );
    assert_eq!(requests[0].method, "POST", "first request defers");
    assert!(
        requests[0].path.ends_with("/callback"),
        "first request is the defer callback, got {}",
        requests[0].path
    );
    let defer: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(defer["type"], 5, "the defer is a deferred response");
    assert_eq!(
        defer["data"]["flags"], 64,
        "the defer is ephemeral: the preview is admin-only"
    );
    assert_eq!(
        callbacks(mock).len(),
        1,
        "the initial acknowledgement is never retried"
    );
}

/// A preview performs zero mutations of any kind: no roles, no posts, no
/// funnel/prompt rows for the fixture or the invoker, no invite/voice/audit
/// traffic.
async fn assert_zero_preview_effects(db: &TestSchema, mock: &MockRest) {
    assert!(
        role_writes(mock).is_empty(),
        "a preview grants/removes no roles, got {:?}",
        role_writes(mock)
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect::<Vec<_>>()
    );
    assert!(
        posts(mock).is_empty(),
        "a preview posts no welcome/farewell"
    );
    assert_eq!(
        event_count(db).await,
        0,
        "a preview records no funnel/selection events"
    );
    for (member, label) in [
        (PREVIEW_FIXTURE_MEMBER_ID.to_string(), "fixture"),
        ("44".to_owned(), "invoker"),
    ] {
        assert!(
            !has_onboarding_prompt(&db.pool, "22", &member)
                .await
                .unwrap(),
            "a preview arms no prompt guard for the {label} ({member})"
        );
    }
    for request in mock.requests() {
        for forbidden in ["invite", "audit-log", "voice"] {
            assert!(
                !request.path.contains(forbidden),
                "a preview touches no {forbidden} route, got {} {}",
                request.method,
                request.path
            );
        }
    }
}

/// The banner: first line of every preview reply (success and welcome-state
/// guidance alike), naming the preview, the non-delivery, the fixture and
/// the mode. Only the non-admin refusal omits it (exact shared sentence).
fn assert_banner(content: &str, mode: &str) {
    let first = content.lines().next().unwrap_or_default();
    let lowered = first.to_lowercase();
    assert!(
        lowered.contains("preview"),
        "the banner names the preview on its first line, got {first:?}"
    );
    assert!(
        lowered.contains("not a delivery"),
        "the banner states this is not a delivery, got {first:?}"
    );
    assert!(
        first.contains(&PREVIEW_FIXTURE_MEMBER_ID.to_string()),
        "the banner names the fixture member id, got {first:?}"
    );
    assert!(
        lowered.contains(mode),
        "the banner names the {mode} mode, got {first:?}"
    );
}

/// The wire-safety half of every rendered preview: bounded, mention-safe,
/// and pinging nobody — the fixture pill is inert text, not a delivery.
fn assert_preview_wire_safe(body: &Value) {
    let content = edit_content(body);
    assert!(
        two_bot_core::message_safety::text_len(content)
            <= two_bot_core::message_safety::CONTENT_LIMIT,
        "the preview fits the Discord content bound"
    );
    assert_eq!(
        body["allowed_mentions"]["parse"],
        Value::Array(vec![]),
        "the preview edit disables mention parsing"
    );
    assert_eq!(
        body["allowed_mentions"]["roles"],
        Value::Array(vec![]),
        "the preview edit mentions no roles"
    );
    assert_eq!(
        body["allowed_mentions"]["users"],
        Value::Array(vec![]),
        "the preview edit pings nobody: it is not a delivery"
    );
    assert!(
        !two_bot_core::message_safety::contains_mass_mention(content),
        "the preview carries no live mass mention: {content:?}"
    );
}

fn assert_no_success_markers(content: &str) {
    for marker in [
        "Done.",
        "On it",
        "Game roles saved.",
        "roles were changed",
        "posted",
    ] {
        assert!(
            !content.contains(marker),
            "a guidance reply claims no success ({marker:?}), got {content:?}"
        );
    }
}

// Live anchor delivery substitutes the configured destination after rendering
// the occurrence. The fixture uses channel 14, not the built-in Sunday channel.
fn configured_anchor_welcome_text() -> String {
    let deployment = vars("anchor", false);
    anchor_welcome_text(PREVIEW_FIXTURE_MEMBER_ID, now_secs(), SUNDAY_SQUAD).replace(
        SUNDAY_SQUAD.channel_id,
        &deployment["DISCORD_ANCHOR_WELCOME_CHANNEL_ID"],
    )
}

fn assert_no_welcome_body(content: &str, mode: &str) {
    let body = match mode {
        "legacy" => legacy_welcome_text(PREVIEW_FIXTURE_MEMBER_ID),
        "session" => session_welcome_text(PREVIEW_FIXTURE_MEMBER_ID),
        "anchor" => configured_anchor_welcome_text(),
        _ => panic!("known mode"),
    };
    assert!(
        !content.contains(&body),
        "a guidance reply renders no {mode} welcome body"
    );
    assert!(
        !content.contains(PREVIEW_FIXTURE_PILL),
        "a guidance reply substitutes no fixture mention"
    );
}

// RED at baseline: the slash is not captured, so nothing defers. Pin the
// whole legacy success shape: capture, ephemeral defer-first, banner, the
// byte-identical live body for the fixture, the existing games renderer
// with no preselected roles, wire safety, zero mutations.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_legacy_renders_live_body_banner_and_games_picker() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        let interaction = preview();
        assert!(
            runtime.accepts_interaction(&interaction),
            "the shared capture admits /welcome-preview in legacy mode"
        );
        runtime.handle(job(interaction), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        let content = edit_content(&body);
        assert_banner(content, "legacy");
        assert!(
            content.contains(&legacy_welcome_text(PREVIEW_FIXTURE_MEMBER_ID)),
            "the preview embeds the byte-identical legacy body for the fixture, got {content:?}"
        );
        assert!(
            content.contains("12"),
            "the preview names the destination landing channel, got {content:?}"
        );
        assert_preview_wire_safe(&body);
        let expected = picker_components(Some(PickerKind::Games), &[], &[]);
        assert_eq!(
            body["components"],
            serde_json::to_value(expected).unwrap(),
            "the preview carries the existing games renderer for a roleless fixture"
        );
        let menu = &body["components"][0]["components"][0];
        assert_eq!(menu["custom_id"], GAME_SELECT_ID);
        assert_eq!(
            menu["options"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|option| option["default"] == true)
                .count(),
            0,
            "a roleless fixture preselects nothing"
        );
        assert_zero_preview_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, no preview. The session preview lists the
// CURRENT configured picks through the existing session renderer.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_session_renders_live_body_and_configured_session_picker() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        let interaction = preview();
        assert!(
            runtime.accepts_interaction(&interaction),
            "the shared capture admits /welcome-preview in session mode"
        );
        runtime.handle(job(interaction), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        let content = edit_content(&body);
        assert_banner(content, "session");
        assert!(
            content.contains(&session_welcome_text(PREVIEW_FIXTURE_MEMBER_ID)),
            "the preview embeds the byte-identical session body for the fixture, got {content:?}"
        );
        assert!(
            content.contains("12"),
            "the preview names the destination landing channel, got {content:?}"
        );
        assert_preview_wire_safe(&body);
        let picks = build_session_picks("10", "11");
        let expected = picker_components(Some(PickerKind::Session), &[], &picks);
        assert_eq!(
            body["components"],
            serde_json::to_value(expected).unwrap(),
            "the preview carries the existing session renderer over current picks"
        );
        let menu = &body["components"][0]["components"][0];
        assert_eq!(menu["custom_id"], SESSION_SELECT_ID);
        let values: Vec<&str> = menu["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, vec!["find-players", "join-voice"]);
        assert_zero_preview_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, no preview. The anchor preview embeds the
// byte-identical anchor body (same occurrence math, same instant) and
// attaches nothing — anchor welcomes never carry a picker.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_anchor_renders_live_body_without_picker() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("anchor", false));
        let interaction = preview();
        assert!(
            runtime.accepts_interaction(&interaction),
            "the shared capture admits /welcome-preview in anchor mode"
        );
        runtime.handle(job(interaction), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        let content = edit_content(&body);
        assert_banner(content, "anchor");
        assert!(
            content.contains(&configured_anchor_welcome_text()),
            "the preview embeds the byte-identical anchor body for the fixture, got {content:?}"
        );
        assert!(
            content.contains("<#14>"),
            "the preview names the configured anchor channel, got {content:?}"
        );
        assert!(
            !content.contains(SUNDAY_SQUAD.channel_id),
            "the preview never appends a stale built-in anchor body, got {content:?}"
        );
        assert_preview_wire_safe(&body);
        assert!(
            body.get("components")
                .is_none_or(|c| c.as_array().is_some_and(Vec::is_empty)),
            "anchor previews attach no picker, got {:?}",
            body.get("components")
        );
        assert_zero_preview_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, no preview. The parity invariant against a
// REAL join: for each mode, a live `Welcome` job for the fixture posts
// through the full sibling pipeline, and a preview for the same config
// embeds that posted body and picker byte-identical. Separate schemas keep
// the join's prompt/funnel writes out of the preview's zero-effects proof.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_matches_live_join_render_for_same_config_across_modes() {
    for mode in ["legacy", "session", "anchor"] {
        let live_db = TestSchema::new().await;
        let live_result = std::panic::AssertUnwindSafe(async {
            let live_mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
            let live_runtime = runtime(&live_db.pool, &live_mock, &vars(mode, false));
            live_runtime
                .handle(welcome(PREVIEW_FIXTURE_MEMBER_ID), NOW)
                .await
                .unwrap();
            let channel_posts = posts(&live_mock);
            assert_eq!(
                channel_posts.len(),
                1,
                "the live {mode} join posts exactly one welcome"
            );
            serde_json::from_slice::<Value>(&channel_posts[0].body).unwrap()
        })
        .catch_unwind()
        .await;
        let live_body = match live_result {
            Ok(live_body) => {
                live_db.close().await;
                live_body
            }
            Err(error) => {
                live_db.close().await;
                std::panic::resume_unwind(error);
            }
        };

        let db = TestSchema::new().await;
        let result = std::panic::AssertUnwindSafe(async {
            let mock =
                preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
            let runtime = runtime(&db.pool, &mock, &vars(mode, false));
            let interaction = preview();
            assert!(
                runtime.accepts_interaction(&interaction),
                "the shared capture admits /welcome-preview in {mode} mode"
            );
            runtime.handle(job(interaction), NOW).await.unwrap();
            assert_deferred_first(&mock);
            let body = single_edit_body(&mock);
            let content = edit_content(&body);
            assert_banner(content, mode);
            let live_content = live_body["content"].as_str().unwrap_or_default();
            assert!(
                !live_content.is_empty(),
                "the live {mode} join posts a body to match"
            );
            assert!(
                content.contains(live_content),
                "the {mode} preview embeds the live join's posted body byte-identical, got {content:?}"
            );
            assert_eq!(
                body.get("components"),
                live_body.get("components"),
                "the {mode} preview carries the live join's picker byte-identical"
            );
            assert_preview_wire_safe(&body);
            assert_zero_preview_effects(&db, &mock).await;
        })
        .catch_unwind()
        .await;
        db.close().await;
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}

// Missing preview-origin fencing must never turn a visual sample into a real
// game-role grant or session-route event. Exercise the actual rendered menus,
// not a new custom ID; broken config must not move this fence behind a lookup.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_rendered_picker_clicks_are_read_only_before_config_lookup() {
    for mode in ["legacy", "session"] {
        for broken_config in [false, true] {
            let db = TestSchema::new().await;
            let result = std::panic::AssertUnwindSafe(async {
                let render_mock =
                    preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
                let runtime = runtime(&db.pool, &render_mock, &vars(mode, false));
                runtime.handle(job(preview()), NOW).await.unwrap();
                assert_deferred_first(&render_mock);
                let body = single_edit_body(&render_mock);
                assert_banner(edit_content(&body), mode);
                assert_zero_preview_effects(&db, &render_mock).await;
                let expected = if mode == "legacy" {
                    picker_components(Some(PickerKind::Games), &[], &[])
                } else {
                    picker_components(
                        Some(PickerKind::Session),
                        &[],
                        &build_session_picks("10", "11"),
                    )
                };
                assert_eq!(
                    body["components"],
                    serde_json::to_value(expected).unwrap(),
                    "the visual sample retains the live {mode} components"
                );
                let click = click_rendered_picker(&body, mode);
                if broken_config {
                    // Seed a corrupt legacy row without the validated settings writer.
                    sqlx::query(
                        "INSERT INTO guild_settings (guild_id, key, value, updated_by) VALUES ($1, $2, $3, $4)",
                    )
                    .bind("22")
                    .bind("DISCORD_LANDING_CHANNEL_IDS")
                    .bind(json!(["bogus"]))
                    .bind("test")
                    .execute(&db.pool)
                    .await
                    .unwrap();
                }
                // A separate strict responder records only the click lifecycle.
                // Any member read/role write on a sample click panics here.
                let click_mock =
                    preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
                let click_runtime = super::runtime(&db.pool, &click_mock, &vars(mode, false));
                assert!(
                    click_runtime.accepts_interaction(&click),
                    "a preview menu click gets private guidance, not silence"
                );
                click_runtime.handle(job(click), NOW + 1).await.unwrap();
                assert_deferred_first(&click_mock);
                let reply = single_edit_body(&click_mock);
                let content = edit_content(&reply);
                assert!(
                    content.to_lowercase().contains("preview"),
                    "the click explains the preview even with broken_config={broken_config}, got {content:?}"
                );
                assert_no_success_markers(content);
                assert_no_welcome_body(content, mode);
                assert_zero_preview_effects(&db, &click_mock).await;
            })
            .catch_unwind()
            .await;
            db.close().await;
            if let Err(error) = result {
                std::panic::resume_unwind(error);
            }
        }
    }
}

// RED at baseline: no capture, no guidance. Dry run is the off-state for
// legacy/anchor (the live renderer skips; the preview shows the skip, not
// the welcome), while session still renders — parity WITH the flag, in
// both directions.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_dry_run_shows_skip_guidance_except_session_renders() {
    for mode in ["legacy", "anchor"] {
        let db = TestSchema::new().await;
        let result = std::panic::AssertUnwindSafe(async {
            let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
            let runtime = runtime(&db.pool, &mock, &vars(mode, true));
            let interaction = preview();
            assert!(
                runtime.accepts_interaction(&interaction),
                "the shared capture admits /welcome-preview in dry-run {mode} mode"
            );
            runtime.handle(job(interaction), NOW).await.unwrap();
            assert_deferred_first(&mock);
            let body = single_edit_body(&mock);
            let content = edit_content(&body);
            assert_banner(content, mode);
            assert!(
                content.to_lowercase().contains("dry run"),
                "the dry-run {mode} preview names the dry-run skip, got {content:?}"
            );
            assert_no_welcome_body(content, mode);
            assert_no_success_markers(content);
            assert!(
                body.get("components")
                    .is_none_or(|c| c.as_array().is_some_and(Vec::is_empty)),
                "a skip guidance attaches no picker"
            );
            assert_zero_preview_effects(&db, &mock).await;
        })
        .catch_unwind()
        .await;
        db.close().await;
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", true));
        let interaction = preview();
        assert!(
            runtime.accepts_interaction(&interaction),
            "the shared capture admits /welcome-preview in dry-run session mode"
        );
        runtime.handle(job(interaction), NOW).await.unwrap();
        let body = single_edit_body(&mock);
        let content = edit_content(&body);
        assert_banner(content, "session");
        assert!(
            content.contains(&session_welcome_text(PREVIEW_FIXTURE_MEMBER_ID)),
            "dry run still renders the session welcome (it never grants roles), got {content:?}"
        );
        assert_zero_preview_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, no guidance. The substitution-gap marker:
// with no landing channel configured, legacy/session cannot substitute the
// destination slot, so the preview names the missing landing channel
// explicitly instead of dropping the welcome silently.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_empty_landing_channels_marks_the_gap() {
    for mode in ["legacy", "session"] {
        let db = TestSchema::new().await;
        let result = std::panic::AssertUnwindSafe(async {
            let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
            let mut deployment = vars(mode, false);
            deployment.insert("DISCORD_LANDING_CHANNEL_IDS".into(), String::new());
            let runtime = runtime(&db.pool, &mock, &deployment);
            let interaction = preview();
            assert!(
                runtime.accepts_interaction(&interaction),
                "the shared capture admits /welcome-preview with no landing channel ({mode})"
            );
            runtime.handle(job(interaction), NOW).await.unwrap();
            assert_deferred_first(&mock);
            let body = single_edit_body(&mock);
            let content = edit_content(&body);
            assert_banner(content, mode);
            assert!(
                content.to_lowercase().contains("landing"),
                "the gap marker names the missing landing channel, got {content:?}"
            );
            assert_no_welcome_body(content, mode);
            assert_no_success_markers(content);
            assert!(
                body.get("components")
                    .is_none_or(|c| c.as_array().is_some_and(Vec::is_empty)),
                "a gap guidance attaches no picker"
            );
            assert_zero_preview_effects(&db, &mock).await;
        })
        .catch_unwind()
        .await;
        db.close().await;
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}

// RED at baseline: no capture, no guidance. Broken stored config fails the
// render closed: hostile values never echo raw, the welcome never renders,
// and the guidance carries retry words. Three breaks, one shared shape.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_broken_stored_config_fails_privately_with_retry_guidance() {
    // Hostile landing ids in legacy mode: mention pills, a mass mention and
    // a link smuggled through stored config.
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        // Seed a corrupt legacy row without the validated settings writer.
        sqlx::query(
            "INSERT INTO guild_settings (guild_id, key, value, updated_by) VALUES ($1, $2, $3, $4)",
        )
        .bind("22")
        .bind("DISCORD_LANDING_CHANNEL_IDS")
        .bind(json!(["@everyone <@&1> https://evil.example/x"]))
        .bind("test")
        .execute(&db.pool)
        .await
        .unwrap();
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        let interaction = preview();
        assert!(
            runtime.accepts_interaction(&interaction),
            "the shared capture admits /welcome-preview before config reads"
        );
        runtime.handle(job(interaction), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        let content = edit_content(&body);
        assert_banner(content, "legacy");
        assert!(
            content.to_lowercase().contains("try again"),
            "broken config fails with retry guidance, got {content:?}"
        );
        for hostile in ["@everyone", "<@&", "evil.example"] {
            assert!(
                !content.contains(hostile),
                "hostile stored config never echoes raw ({hostile:?}), got {content:?}"
            );
        }
        assert_no_welcome_body(content, "legacy");
        assert_no_success_markers(content);
        assert_zero_preview_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }

    // Emptied anchor channel in anchor mode: the mode requires it.
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        SettingsStore::new(&db.pool)
            .set(
                "22",
                "DISCORD_ANCHOR_WELCOME_CHANNEL_ID",
                Some(json!([""])),
                "test",
            )
            .await
            .unwrap();
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("anchor", false));
        runtime.handle(job(preview()), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        let content = edit_content(&body);
        assert_banner(content, "anchor");
        assert!(
            content.to_lowercase().contains("try again"),
            "a mode-required channel missing from config fails with retry guidance, got {content:?}"
        );
        assert_no_welcome_body(content, "anchor");
        assert_no_success_markers(content);
        assert_zero_preview_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }

    // Non-snowflake session pick in session mode.
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        // Seed a corrupt legacy row without the validated settings writer.
        sqlx::query(
            "INSERT INTO guild_settings (guild_id, key, value, updated_by) VALUES ($1, $2, $3, $4)",
        )
        .bind("22")
        .bind("DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID")
        .bind(json!(["bogus"]))
        .bind("test")
        .execute(&db.pool)
        .await
        .unwrap();
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime.handle(job(preview()), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        let content = edit_content(&body);
        assert_banner(content, "session");
        assert!(
            content.to_lowercase().contains("try again"),
            "a non-snowflake session pick fails with retry guidance, got {content:?}"
        );
        assert!(
            !content.contains("bogus"),
            "the offending stored value never echoes raw, got {content:?}"
        );
        assert_no_welcome_body(content, "session");
        assert_no_success_markers(content);
        assert_zero_preview_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, no guidance. The renderer outage: the
// settings store dies mid-flight, so the preview guides privately with
// retry words, sends the ack exactly once, and writes nothing anywhere.
// DB row assertions are impossible on a closed pool, so this test pins the
// wire half only: no role writes, no posts, one guidance edit.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_store_outage_guides_privately_without_ack_retry() {
    let db = TestSchema::new().await;
    let pool = db.pool.clone();
    let result = std::panic::AssertUnwindSafe(async {
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState {
            close_pool_on_callback: Some(pool),
            ..PreviewDiscordState::default()
        })))
        .await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        let interaction = preview();
        assert!(
            runtime.accepts_interaction(&interaction),
            "the shared capture admits /welcome-preview before the outage lands"
        );
        runtime.handle(job(interaction), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        let content = edit_content(&body);
        assert_banner(content, "legacy");
        assert!(
            content.to_lowercase().contains("try again"),
            "a store outage fails with retry guidance, got {content:?}"
        );
        assert_no_welcome_body(content, "legacy");
        assert_no_success_markers(content);
        assert!(
            role_writes(&mock).is_empty(),
            "an outage preview writes no roles"
        );
        assert!(posts(&mock).is_empty(), "an outage preview posts nothing");
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: the slash is not captured, so nothing refuses. Capture
// admits the invocation (guild + human member present) and the handler
// refuses privately with the exact shared Manage Server sentence — zero
// permission bits and missing resolved permissions alike. Zero writes.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_non_admin_refused_privately_with_zero_writes() {
    for member in [admin_member("44", Some("0")), admin_member("44", None)] {
        let db = TestSchema::new().await;
        let result = std::panic::AssertUnwindSafe(async {
            let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
            let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
            let interaction = preview_with_member(member);
            assert!(
                runtime.accepts_interaction(&interaction),
                "capture admits the invocation so the refusal is private, not silence"
            );
            runtime.handle(job(interaction), NOW).await.unwrap();
            assert_deferred_first(&mock);
            let body = single_edit_body(&mock);
            assert_eq!(
                edit_content(&body),
                manage_server_refusal(),
                "non-admins get the exact shared Manage Server refusal"
            );
            assert_no_success_markers(edit_content(&body));
            assert_zero_preview_effects(&db, &mock).await;
        })
        .catch_unwind()
        .await;
        db.close().await;
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}

// Control: the capture fence stays first — foreign guilds, bots and
// memberless payloads are silent no-ops, today and after the build.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn preview_wrong_guild_bot_and_memberless_stay_silent() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        for (interaction, label) in [
            (preview_in_guild("23"), "foreign guild"),
            (preview_by_bot(), "bot invoker"),
            (preview_without_member(), "memberless payload"),
        ] {
            assert!(
                !runtime.accepts_interaction(&interaction),
                "{label} is not captured"
            );
            runtime.handle(job(interaction), NOW).await.unwrap();
            assert!(
                mock.requests().is_empty(),
                "{label} sends nothing, got {} requests",
                mock.requests().len()
            );
        }
        assert_zero_preview_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// A saturated shared command lane must yield ownership before it can send a
// competing busy ACK. Occupancy is deterministic; no database or timers fill it.
#[tokio::test]
async fn preview_shared_dispatch_yields_before_busy_admission() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/postgres")
        .expect("offline lazy pool");
    pool.close().await;
    let busy_seen = Arc::new(tokio::sync::Notify::new());
    let signal = Arc::clone(&busy_seen);
    let mock = MockRest::with_responder(move |request: &RestRequest| {
        assert_eq!(request.method, "POST");
        assert!(request.path.ends_with("/callback"));
        signal.notify_one();
        ScriptedResponse::status(204)
    })
    .await;
    let shared = shared_preview_runtime(&pool, &mock);
    let _dispatch_guard = shared.dispatch_guard();
    let _slots: Vec<_> = (0..crate::interaction_admission::PER_USER_IN_FLIGHT)
        .map(|_| {
            shared
                .acquire_user_slot_for_test(44)
                .expect("hold actor slot")
        })
        .collect();
    let mut invocation = preview();
    // The shared test constructor pins this application; a mismatched fixture
    // would silently bypass busy admission and make the oracle vacuous.
    invocation.application_id = twilight_model::id::Id::new(1111);
    let event = Event::InteractionCreate(Box::new(
        twilight_model::gateway::payload::incoming::InteractionCreate(invocation),
    ));
    assert!(
        shared.dispatch(&event),
        "the shared runtime yields welcome-preview before its exhausted actor budget"
    );
    // The same actor's ordinary slash must still hit busy admission. This
    // positive control rules out globally skipping the shared admission gate.
    let mut ordinary = serde_json::to_value(preview()).unwrap();
    ordinary["application_id"] = json!("1111");
    ordinary["id"] = json!("335");
    ordinary["token"] = json!("mock-busy-control");
    ordinary["data"]["name"] = json!("help");
    let ordinary = serde_json::from_value(ordinary).expect("ordinary slash control");
    let ordinary = Event::InteractionCreate(Box::new(
        twilight_model::gateway::payload::incoming::InteractionCreate(ordinary),
    ));
    assert!(!shared.dispatch(&ordinary), "the ordinary slash is busy");
    tokio::time::timeout(Duration::from_secs(2), busy_seen.notified())
        .await
        .expect("the ordinary slash receives its busy callback");
    let requests = callbacks(&mock);
    assert_eq!(requests.len(), 1, "only the ordinary slash receives an ACK");
    assert!(requests[0].path.contains("/335/mock-busy-control/"));
    let reply: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64);
    assert_eq!(
        reply["data"]["content"],
        crate::interaction_admission::BUSY_REPLY
    );
}

// Both gateway consumers see the slash, but only onboarding may acknowledge.
// A closed lazy pool forces post-defer guidance without any external database.
#[tokio::test]
async fn preview_shared_handler_and_onboarding_send_only_one_ack() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/postgres")
        .expect("offline lazy pool");
    pool.close().await;
    let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
    let shared = shared_preview_runtime(&pool, &mock);
    let _dispatch_guard = shared.dispatch_guard();
    let runtime = runtime(&pool, &mock, &vars("legacy", false));
    let mut invocation = preview();
    invocation.application_id = twilight_model::id::Id::new(1111);
    shared.on_interaction(&invocation).await;
    assert!(
        mock.requests().is_empty(),
        "the shared handler sends no callback before onboarding owns the slash"
    );
    runtime.handle(job(invocation), NOW).await.unwrap();
    assert_deferred_first(&mock);
    let reply = single_edit_body(&mock);
    let content = edit_content(&reply);
    assert_banner(content, "legacy");
    assert!(content.to_lowercase().contains("try again"));
    assert_no_welcome_body(content, "legacy");
    assert_no_success_markers(content);
    assert!(role_writes(&mock).is_empty());
    assert!(posts(&mock).is_empty());
}

// Control (offline): the fixture identity is fixed and documented, and
// boot config with a bogus mode fails closed at construction — a broken
// deployment never builds a runtime that could misrender. Tokio (not ignore):
// dropping the lazy pool requires a runtime context, but no connection is
// ever opened, so this stays in the default offline lane.
#[tokio::test]
async fn preview_fixture_identity_and_fail_closed_construction() {
    assert_eq!(
        PREVIEW_FIXTURE_MEMBER_ID, 765_432_109_876_543_210,
        "the fixture id is pinned:docs and builder share it"
    );
    assert_eq!(PREVIEW_FIXTURE_PILL, "<@765432109876543210>");
    let anchor = configured_anchor_welcome_text();
    assert!(
        anchor.contains("<#14>"),
        "the anchor oracle uses fixture config"
    );
    assert!(
        !anchor.contains(SUNDAY_SQUAD.channel_id),
        "the anchor oracle excludes the stale built-in destination"
    );
    for (body, mode) in [
        (legacy_welcome_text(PREVIEW_FIXTURE_MEMBER_ID), "legacy"),
        (session_welcome_text(PREVIEW_FIXTURE_MEMBER_ID), "session"),
        (configured_anchor_welcome_text(), "anchor"),
    ] {
        assert_eq!(
            body.matches("<@").count(),
            1,
            "the {mode} fixture body substitutes exactly one member mention"
        );
        assert!(
            body.contains(PREVIEW_FIXTURE_PILL),
            "the {mode} fixture body mentions the fixture, got {body:?}"
        );
    }
    // A bogus deployment mode never constructs: use a lazy pool (no
    // connection is opened) and a loopback-only executor (no requests).
    // A production origin correctly requires durable send admission.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/postgres")
        .expect("lazy pool builds offline");
    ensure_crypto_provider();
    let executor = ActionExecutor::with_proxy(
        "welcome-preview-test-only-token".into(),
        Some("http://127.0.0.1:1".into()),
    )
    .expect("loopback-only executor builds offline");
    let mut deployment = vars("bogus", false);
    deployment.insert("DISCORD_GUILD_ID".into(), "22".into());
    assert!(
        OnboardingRuntime::new(pool, executor, &deployment, 22, 999).is_err(),
        "a bogus TWO_ONBOARDING_MODE fails closed at construction"
    );
}
