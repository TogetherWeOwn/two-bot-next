//! Acceptance: members reopen their existing game/session pickers privately.
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
//! SessionPicker components only, and several picker replies tell the
//! member to "open the menu and try again" — but no command opens a menu.
//! The fix routes ONE self-service slash command (`/reopen`, kind
//! games/session — invocation shape pinned in the core crate's frozen
//! `onboarding_reopen_picker_acceptance`) through the EXISTING onboarding
//! pipeline:
//!
//! - the shared router admits it (configured guild, always-on, open to
//!   everyone; wrong guild, bots and memberless payloads stay silent);
//! - the existing receive-relative ephemeral acknowledge runs BEFORE any
//!   settings/member read, with no retry of an unconfirmed ack;
//! - a fresh menu is built from CURRENT member roles and CURRENT configured
//!   picks through the existing `picker_components` renderer — games where
//!   the game picker is live (legacy/anchor, mirroring the component gates),
//!   session where the session picker is live (session mode);
//! - opening a menu is read-only: zero role grants/removals, channel posts,
//!   funnel/prompt records, invites or voice moves;
//! - disabled/unavailable kinds, malformed invocations and failed lookups
//!   get exactly one private guidance edit reusing the established
//!   no-effect vocabulary — never a menu, never a success claim;
//! - the reopened menu's own select feeds the EXISTING component handler,
//!   with unchanged authorization, dry-run, replay and role effects.
//!
//! RED at baseline (10 tests): the slash is not captured, so every handle
//! below is a silent no-op — no defer, no menu, no guidance.
//!
//! Controls (4 tests): guild/bot/memberless silence, the existing game
//! selection path, and the interrupted-interaction non-replay fence.
//!
//! This file brings its own mock Discord responder so fixture changes in
//! the sibling runtime tests cannot change what is pinned here. It uses the
//! sibling `TestSchema`, `runtime`, `vars`, `component`, `replies`, `posts`
//! and `has_onboarding_prompt` helpers verbatim; the builder must keep
//! those signatures stable.
//!
//! Disposable-DB only (plus one offline test): no live Discord, no
//! staging/production database, never `DATABASE_URL`.

use super::*;
use two_bot_discord::onboarding_messages::picker_components;

/// Reused terminal copy: a post-defer configuration failure keeps the exact
/// existing sentence (onboarding.rs `handle_acknowledged`).
const CONFIG_FAILURE_REPLY: &str = "I couldn't load the menu configuration. No selection was applied. Please open the menu and try again later.";
/// Reused terminal copy: an unconfirmed initial acknowledgement keeps the
/// exact existing sentence (onboarding.rs `unconfirmed_interaction`).
const UNCONFIRMED_REPLY: &str = "I couldn't confirm this interaction. Your selection may already have been processed, so I won't apply it again. Please open the menu and try again.";

#[derive(Default)]
struct ReopenDiscordState {
    roles: HashSet<String>,
    reject_callback: bool,
    fail_member_reads: bool,
    close_pool_on_callback: Option<PgPool>,
}

async fn reopen_discord(state: Arc<Mutex<ReopenDiscordState>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let mut state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/guilds/22") => {
                ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"}))
            }
            ("GET", "/guilds/22/roles") => {
                let mut roles = vec![
                    json!({"id":"22","permissions":"1051648"}),
                    json!({"id":"77","permissions":"0"}),
                ];
                roles.extend(
                    GAME_PICKS
                        .iter()
                        .chain(PLATFORM_PICKS.iter())
                        .map(|pick| json!({"id":pick.role_id,"permissions":"0"})),
                );
                ScriptedResponse::json(200, Value::Array(roles))
            }
            ("GET", path) if path.starts_with("/guilds/22/members/") => {
                let id = path.strip_prefix("/guilds/22/members/").unwrap();
                if state.fail_member_reads && id == "44" {
                    return ScriptedResponse::status(403);
                }
                let roles: Vec<String> = if id == "999" {
                    vec![]
                } else {
                    state.roles.iter().cloned().collect()
                };
                ScriptedResponse::json(200, json!({"user":{"id":id},"roles":roles}))
            }
            ("GET", path) if path.starts_with("/channels/") => {
                let id = path.strip_prefix("/channels/").unwrap();
                ScriptedResponse::json(
                    200,
                    json!({"id":id,"guild_id":"22","type":0,"permission_overwrites":[]}),
                )
            }
            ("PUT" | "DELETE", path) if path.contains("/roles/") => {
                let role = path.rsplit('/').next().unwrap().to_owned();
                if request.method == "PUT" {
                    state.roles.insert(role);
                } else {
                    state.roles.remove(&role);
                }
                ScriptedResponse::status(204)
            }
            ("POST", path) if path.starts_with("/channels/") && path.ends_with("/messages") => {
                ScriptedResponse::json(201, json!({"id":"99"}))
            }
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
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

fn member_value(user_id: &str, bot: bool) -> Value {
    json!({
        "permissions": "0", "roles": [], "deaf": false, "mute": false, "flags": 0,
        "user": {"id": user_id, "username": "member", "discriminator": "0", "bot": bot},
    })
}

fn kind_option(kind: &str) -> Value {
    json!([{"name": "kind", "type": 3, "value": kind}])
}

/// A `/reopen` slash invocation from human member 44 in guild 22, through
/// the same wire shape the gateway delivers.
fn reopen(kind: &str) -> Interaction {
    reopen_with_options(kind_option(kind))
}

fn reopen_with_options(options: Value) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": "22"},
        "id": "333", "token": "mock-callback", "type": 2, "version": 1, "guild_id": "22",
        "member": member_value("44", false),
        "data": {"id": "444", "name": "reopen", "type": 1, "options": options},
    }))
    .expect("valid reopen slash fixture")
}

fn reopen_in_guild(guild_id: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": guild_id},
        "id": "333", "token": "mock-callback", "type": 2, "version": 1, "guild_id": guild_id,
        "member": member_value("44", false),
        "data": {"id": "444", "name": "reopen", "type": 1, "options": kind_option("games")},
    }))
    .expect("valid reopen slash fixture")
}

fn reopen_by_bot() -> Interaction {
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": "22"},
        "id": "333", "token": "mock-callback", "type": 2, "version": 1, "guild_id": "22",
        "member": member_value("44", true),
        "data": {"id": "444", "name": "reopen", "type": 1, "options": kind_option("games")},
    }))
    .expect("valid reopen slash fixture")
}

fn reopen_without_member() -> Interaction {
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": "22"},
        "id": "333", "token": "mock-callback", "type": 2, "version": 1, "guild_id": "22",
        "member": Value::Null,
        "data": {"id": "444", "name": "reopen", "type": 1, "options": kind_option("games")},
    }))
    .expect("valid reopen slash fixture")
}

fn job(interaction: Interaction) -> OnboardingJob {
    OnboardingJob::Interaction(Box::new(interaction))
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

fn reply_contents(mock: &MockRest) -> Vec<String> {
    replies(mock)
        .iter()
        .map(|reply| {
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            body["content"].as_str().unwrap_or_default().to_owned()
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

async fn event_count(db: &TestSchema) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

/// The defer-first half of every success: the FIRST request is the
/// ephemeral defer, before any settings/member read.
fn assert_deferred_first(mock: &MockRest) {
    let requests = mock.requests();
    assert!(
        !requests.is_empty(),
        "the reopen is captured and deferred before any read"
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
        "the defer is ephemeral: the menu is self-only"
    );
    assert_eq!(
        callbacks(mock).len(),
        1,
        "the initial acknowledgement is never retried"
    );
}

/// Opening a menu performs zero mutations of any kind.
async fn assert_zero_menu_effects(db: &TestSchema, mock: &MockRest) {
    assert!(
        role_writes(mock).is_empty(),
        "opening a menu grants/removes no roles, got {:?}",
        role_writes(mock)
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect::<Vec<_>>()
    );
    assert!(
        posts(mock).is_empty(),
        "opening a menu posts no welcome/farewell"
    );
    assert_eq!(
        event_count(db).await,
        0,
        "opening a menu records no funnel/selection events"
    );
    assert!(
        !has_onboarding_prompt(&db.pool, "22", "44").await.unwrap(),
        "opening a menu arms no prompt guard"
    );
}

fn role_id_for(key: &str) -> String {
    GAME_PICKS
        .iter()
        .find(|pick| pick.key == key)
        .unwrap_or_else(|| panic!("catalog keeps the {key} pick"))
        .role_id
        .to_owned()
}

/// Menu edits are mention-safe, like the existing component edit: no mention
/// markup in the content and parsing disabled on the wire.
fn assert_mention_safe(body: &Value) {
    let content = body["content"].as_str().unwrap_or_default();
    assert!(!content.is_empty(), "the menu carries a caption");
    for markup in ["<@", "@everyone", "@here"] {
        assert!(
            !content.contains(markup),
            "menu content carries no mention markup ({markup:?}), got {content:?}"
        );
    }
    assert_eq!(
        body["allowed_mentions"]["parse"],
        Value::Array(vec![]),
        "the menu edit disables mention parsing"
    );
}

fn assert_no_success_markers(content: &str) {
    for marker in ["Done.", "On it", "Game roles saved.", "<#"] {
        assert!(
            !content.contains(marker),
            "a guidance reply claims no success ({marker:?}), got {content:?}"
        );
    }
}

// RED at baseline: the slash is not captured, so nothing defers. Pin the
// whole success shape: capture, ephemeral defer-first, current-role
// defaults through the existing renderer, mention safety, zero mutations.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_games_menu_opens_privately_with_current_role_defaults() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        for mode in ["legacy", "anchor"] {
            let shooters = role_id_for("shooters");
            let state = Arc::new(Mutex::new(ReopenDiscordState {
                roles: HashSet::from([shooters.clone()]),
                ..ReopenDiscordState::default()
            }));
            let mock = reopen_discord(Arc::clone(&state)).await;
            let runtime = runtime(&db.pool, &mock, &vars(mode, false));
            let interaction = reopen("games");
            assert!(
                runtime.accepts_interaction(&interaction),
                "the shared capture admits /reopen in {mode} mode"
            );
            runtime.handle(job(interaction), NOW).await.unwrap();
            assert_deferred_first(&mock);
            let body = single_edit_body(&mock);
            assert_mention_safe(&body);
            let expected = picker_components(Some(PickerKind::Games), &[shooters.as_str()], &[]);
            assert_eq!(
                body["components"],
                serde_json::to_value(expected).unwrap(),
                "the menu is the existing games renderer over current roles ({mode})"
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
                1,
                "exactly the member's current game role preselects ({mode})"
            );
            assert_zero_menu_effects(&db, &mock).await;
        }
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, no menu. The session menu lists the current
// configured picks; accessibility/Connect enforcement stays at selection
// time in the existing handler.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_session_menu_lists_current_configured_choices() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = reopen_discord(Arc::new(Mutex::new(ReopenDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        let interaction = reopen("session");
        assert!(
            runtime.accepts_interaction(&interaction),
            "the shared capture admits /reopen in session mode"
        );
        runtime.handle(job(interaction), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        assert_mention_safe(&body);
        let picks = build_session_picks("10", "11");
        let expected = picker_components(Some(PickerKind::Session), &[], &picks);
        assert_eq!(
            body["components"],
            serde_json::to_value(expected).unwrap(),
            "the menu is the existing session renderer over current picks"
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
        assert_zero_menu_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, no menus. Every open re-reads the member —
// a stale role snapshot from an earlier open must not leak into a later one.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_games_rereads_roles_on_every_open() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let state = Arc::new(Mutex::new(ReopenDiscordState::default()));
        let mock = reopen_discord(Arc::clone(&state)).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        runtime.handle(job(reopen("games")), NOW).await.unwrap();
        let body: Value = {
            let edits = replies(&mock);
            assert_eq!(edits.len(), 1, "the first open edits one menu");
            serde_json::from_slice(&edits[0].body).unwrap()
        };
        assert!(
            body["components"][0]["components"][0]["options"]
                .as_array()
                .unwrap()
                .iter()
                .all(|option| option["default"] != true),
            "no defaults before the member holds a game role"
        );
        state.lock().unwrap().roles.insert(role_id_for("shooters"));
        runtime.handle(job(reopen("games")), NOW + 1).await.unwrap();
        let edits = replies(&mock);
        assert_eq!(edits.len(), 2, "the second open edits a second menu");
        let body: Value = serde_json::from_slice(&edits[1].body).unwrap();
        let defaults: Vec<&str> = body["components"][0]["components"][0]["options"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|option| option["default"] == true)
            .map(|option| option["value"].as_str().unwrap())
            .collect();
        assert_eq!(
            defaults,
            vec!["shooters"],
            "the second open reflects the current roles, not a stale snapshot"
        );
        assert_eq!(callbacks(&mock).len(), 2, "each open defers exactly once");
        assert_zero_menu_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, so no guidance either. A kind the current
// mode does not serve gets exactly one private no-effect edit — never a
// menu, never a success claim — mirroring the stale-menu vocabulary.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_disabled_kind_guides_without_a_menu() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        for (mode, kind) in [("session", "games"), ("legacy", "session")] {
            let mock = reopen_discord(Arc::new(Mutex::new(ReopenDiscordState::default()))).await;
            let runtime = runtime(&db.pool, &mock, &vars(mode, false));
            runtime.handle(job(reopen(kind)), NOW).await.unwrap();
            assert_deferred_first(&mock);
            let body = single_edit_body(&mock);
            assert!(
                body.get("components")
                    .is_none_or(|c| c.as_array().is_none_or(|components| components.is_empty())),
                "a disabled kind renders no menu ({mode}/{kind})"
            );
            let content = body["content"].as_str().unwrap_or_default().to_owned();
            assert!(
                content.contains("enabled"),
                "the guidance names the enablement ({mode}/{kind}), got {content:?}"
            );
            assert!(
                content.contains("No selection was applied"),
                "the guidance carries the no-effect marker ({mode}/{kind}), got {content:?}"
            );
            assert!(
                content.contains("try again"),
                "the guidance offers a retry ({mode}/{kind}), got {content:?}"
            );
            assert_no_success_markers(&content);
            assert_zero_menu_effects(&db, &mock).await;
        }
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, so malformed invocations vanish silently
// instead of guiding. The guidance names both valid choices.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_missing_or_unknown_kind_names_both_choices() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        for options in [Value::Array(vec![]), kind_option("ranked")] {
            let mock = reopen_discord(Arc::new(Mutex::new(ReopenDiscordState::default()))).await;
            let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
            runtime
                .handle(job(reopen_with_options(options.clone())), NOW)
                .await
                .unwrap();
            assert_deferred_first(&mock);
            let body = single_edit_body(&mock);
            assert!(
                body.get("components")
                    .is_none_or(|c| c.as_array().is_none_or(|components| components.is_empty())),
                "a malformed invocation renders no menu, got options {options:?}"
            );
            let content = body["content"].as_str().unwrap_or_default().to_owned();
            assert!(
                content.contains("games") && content.contains("session"),
                "the guidance names both valid kinds, got {content:?}"
            );
            assert!(
                content.contains("try again"),
                "the guidance offers a retry, got {content:?}"
            );
            assert_no_success_markers(&content);
            assert_zero_menu_effects(&db, &mock).await;
        }
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, so a settings outage behind the slash is
// silent. The outage lands inside the post-defer boundary and reuses the
// exact menu-configuration sentence — terminal, with no role or success
// writes. (The pool is dead by design here, so zero-effect is asserted on
// the wire only.)
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_config_outage_reuses_the_menu_configuration_reply() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = reopen_discord(Arc::new(Mutex::new(ReopenDiscordState {
            close_pool_on_callback: Some(db.pool.clone()),
            ..ReopenDiscordState::default()
        })))
        .await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        runtime.handle(job(reopen("games")), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let contents = reply_contents(&mock);
        assert_eq!(
            contents,
            vec![CONFIG_FAILURE_REPLY.to_owned()],
            "a settings outage reuses the menu-configuration reply"
        );
        let body = single_edit_body(&mock);
        assert!(
            body.get("components")
                .is_none_or(|c| c.as_array().is_none_or(|components| components.is_empty())),
            "a settings outage renders no menu"
        );
        assert!(role_writes(&mock).is_empty(), "no role writes on outage");
        assert!(posts(&mock).is_empty(), "no channel posts on outage");
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, no defer, no guidance. A failed member
// lookup after a confirmed defer guides privately — exactly one ack (never
// retried), one edit, no menu, no writes, no success claim.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_member_outage_guides_privately_without_retrying_the_ack() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = reopen_discord(Arc::new(Mutex::new(ReopenDiscordState {
            fail_member_reads: true,
            ..ReopenDiscordState::default()
        })))
        .await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        runtime.handle(job(reopen("games")), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        assert!(
            body.get("components")
                .is_none_or(|c| c.as_array().is_none_or(|components| components.is_empty())),
            "a member outage renders no menu"
        );
        let content = body["content"].as_str().unwrap_or_default().to_owned();
        assert!(
            content.contains("try again"),
            "the guidance offers a retry, got {content:?}"
        );
        assert_no_success_markers(&content);
        assert!(
            !content.contains(GAME_SELECT_ID) && !content.contains(SESSION_SELECT_ID),
            "the guidance leaks no menu identity, got {content:?}"
        );
        assert_zero_menu_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, so an ambiguous ack vanishes with zero
// requests. The unconfirmed path answers once, retries nothing, reads
// nothing, and claims nothing was applied twice.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_unconfirmed_ack_never_retries_and_claims_nothing() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = reopen_discord(Arc::new(Mutex::new(ReopenDiscordState {
            reject_callback: true,
            ..ReopenDiscordState::default()
        })))
        .await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        runtime.handle(job(reopen("games")), NOW).await.unwrap();
        assert_eq!(
            callbacks(&mock).len(),
            1,
            "an unconfirmed ack is never retried"
        );
        let contents = reply_contents(&mock);
        assert_eq!(
            contents,
            vec![UNCONFIRMED_REPLY.to_owned()],
            "an ambiguous ack reuses the unconfirmed reply"
        );
        for request in mock.requests() {
            assert!(
                (request.method == "POST" && request.path.ends_with("/callback"))
                    || request.method == "PATCH",
                "the unconfirmed path performs no reads or mutations, got {} {}",
                request.method,
                request.path
            );
        }
        assert_zero_menu_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no menu opens, so there is no select to submit. The
// reopened menu's own select id feeds the existing component handler with
// unchanged effects — one integration, not a second mutation writer.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_then_existing_game_selection_reuses_the_picker_path() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let state = Arc::new(Mutex::new(ReopenDiscordState::default()));
        let mock = reopen_discord(Arc::clone(&state)).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        runtime.handle(job(reopen("games")), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        let custom_id = body["components"][0]["components"][0]["custom_id"]
            .as_str()
            .expect("the reopened menu carries its select id")
            .to_owned();
        assert_eq!(custom_id, GAME_SELECT_ID);
        assert_eq!(
            event_count(&db).await,
            0,
            "the reopen itself records nothing before the selection"
        );
        runtime
            .handle(component(&custom_id, &["shooters"]), NOW + 1)
            .await
            .unwrap();
        assert!(
            state
                .lock()
                .unwrap()
                .roles
                .contains(&role_id_for("shooters")),
            "the existing handler grants the selected role"
        );
        let contents = reply_contents(&mock);
        assert_eq!(contents.len(), 2, "menu then selection reply");
        assert!(
            contents[1].contains("Done."),
            "the selection keeps the existing success reply, got {:?}",
            contents[1]
        );
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 1);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: no capture, no menu. Rehearsal mode still opens the
// read-only menu (dry-run suppresses proactive public welcomes, not a
// member's own private read) with zero writes.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_games_menu_in_dry_run_opens_with_zero_writes() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let shooters = role_id_for("shooters");
        let state = Arc::new(Mutex::new(ReopenDiscordState {
            roles: HashSet::from([shooters.clone()]),
            ..ReopenDiscordState::default()
        }));
        let mock = reopen_discord(Arc::clone(&state)).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", true));
        runtime.handle(job(reopen("games")), NOW).await.unwrap();
        assert_deferred_first(&mock);
        let body = single_edit_body(&mock);
        let expected = picker_components(Some(PickerKind::Games), &[shooters.as_str()], &[]);
        assert_eq!(
            body["components"],
            serde_json::to_value(expected).unwrap(),
            "dry-run still opens the read-only menu"
        );
        assert_zero_menu_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Control: the configured-guild fence binds the new command — a foreign
// guild stays silent with zero requests.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_wrong_guild_stays_silent_control() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = reopen_discord(Arc::new(Mutex::new(ReopenDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        let interaction = reopen_in_guild("23");
        assert!(
            !runtime.accepts_interaction(&interaction),
            "a foreign guild is never captured"
        );
        runtime.handle(job(interaction), NOW).await.unwrap();
        assert!(
            mock.requests().is_empty(),
            "a foreign guild triggers zero requests"
        );
        assert_zero_menu_effects(&db, &mock).await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Control: bot and memberless invocations are never captured and stay
// silent with zero requests.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_bot_and_memberless_invocations_stay_silent_control() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        for interaction in [reopen_by_bot(), reopen_without_member()] {
            let mock = reopen_discord(Arc::new(Mutex::new(ReopenDiscordState::default()))).await;
            let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
            assert!(
                !runtime.accepts_interaction(&interaction),
                "a bot/memberless invocation is never captured"
            );
            runtime.handle(job(interaction), NOW).await.unwrap();
            assert!(
                mock.requests().is_empty(),
                "a bot/memberless invocation triggers zero requests"
            );
        }
        assert_eq!(
            event_count(&db).await,
            0,
            "silent invocations record nothing"
        );
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Control: the existing game selection path is unchanged — the reopen work
// adds a menu opener, not a second mutation writer.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn existing_game_selection_still_grants_and_records_control() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let state = Arc::new(Mutex::new(ReopenDiscordState::default()));
        let mock = reopen_discord(Arc::clone(&state)).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        runtime
            .handle(component(GAME_SELECT_ID, &["shooters"]), NOW)
            .await
            .unwrap();
        let contents = reply_contents(&mock);
        assert_eq!(contents.len(), 1);
        assert!(
            contents[0].contains("Done."),
            "the success reply is unchanged, got {:?}",
            contents[0]
        );
        assert!(
            state
                .lock()
                .unwrap()
                .roles
                .contains(&role_id_for("shooters")),
            "the role was granted"
        );
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 1);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Control (offline): an interrupted interaction still serializes to a
// non-replayable marker — reopening never blindly replays an uncertain
// selection, it opens a fresh menu for a fresh submission.
#[test]
fn interrupted_interactions_stay_non_replayable_control() {
    let interaction = reopen("games");
    let payload = OnboardingJob::Interaction(Box::new(interaction))
        .durable_payload()
        .expect("the marker serializes");
    assert!(
        payload.contains("interrupted_interaction"),
        "the payload is the non-replayable marker, got {payload}"
    );
    assert!(
        OnboardingJob::recover(&payload)
            .expect("the marker parses")
            .is_none(),
        "an interrupted interaction never resumes as a job"
    );
}
