//! Protected acceptance: pre-submit clarity for game-role apply, clear and preview.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the RED tests pass by changing
//! production copy only (the games picker caption in `onboarding.rs` and the
//! two games-menu placeholders in `onboarding_messages.rs`). Do not edit the
//! fixtures or assertions: they are the spec.
//!
//! The missing behavior: the private Games reopen/picker response captions the
//! picker only as "Pick what you play, then submit", labels the applying menu
//! "What do you play?" and the separate preview menu "Preview proposed
//! changes…". Nothing says which menu changes roles, that unselected catalog
//! game roles are removed, that empty selection clears held catalog game
//! roles, or that preview is preview-only with no role changes. Selection
//! itself submits; there is no separate submit button.
//!
//! The fix, in one copy-only slice:
//! - label the applying game menu as Apply game roles;
//! - explain in the private picker caption that unselected catalog game roles
//!   are removed and empty selection clears held catalog game roles;
//! - label the separate preview menu as preview-only with no role changes;
//! - use short accessible language without implying a submit button exists.
//!
//! RED at baseline (5 tests): both placeholders and the caption lack the
//! required words, and the old copy is still present.
//!
//! Controls (6 tests): menu IDs, min/max, defaults, catalog, bounds,
//! privacy, zero mutations, actual clear/dry-run policy and the existing
//! preview-result next-action text are unchanged.
//!
//! This file brings its own mock Discord responder so fixture changes in
//! sibling runtime tests cannot change what is pinned here. It uses the
//! sibling `runtime`, `vars`, `replies`, `posts` and `has_onboarding_prompt`
//! helpers verbatim; the builder must keep those signatures stable.
//!
//! Disposable-DB only (plus pure wire/policy tests): no live Discord, no
//! staging/production database, never `DATABASE_URL`.
#![cfg(test)]

use super::*;
use std::panic::AssertUnwindSafe;
use two_bot_discord::onboarding_messages::picker_components;
use two_bot_testsupport::TestDatabase;

/// Old applying-menu placeholder at baseline. Mutant detector: this exact
/// question must be gone once the menu names its apply effect.
const OLD_APPLY_PLACEHOLDER: &str = "What do you play?";
/// Old preview-menu placeholder at baseline. Mutant detector: bare "proposed
/// changes" without a no-change promise must be gone.
const OLD_PREVIEW_PLACEHOLDER: &str = "Preview proposed changes…";
/// Old games-picker caption at baseline. Mutant detector: a pick-then-submit
/// sentence with no apply/clear/preview explanation must be gone.
const OLD_GAMES_CAPTION: &str =
    "Here's your games picker — only you can see this menu. Pick what you play, then submit.";
/// Protected preview-result next-action sentence (onboarding.rs
/// `render_game_preview`). Not a new gap: this exact text must survive.
const PREVIEW_NEXT_ACTION: &str =
    "To apply these choices, use the games picker above or open /reopen games again.";

fn lower(text: &str) -> String {
    text.to_lowercase()
}

fn games_wire(member_roles: &[&str]) -> Value {
    serde_json::to_value(picker_components(
        Some(PickerKind::Games),
        member_roles,
        &[],
    ))
    .expect("games picker serializes")
}

fn menu_by_id<'a>(wire: &'a Value, custom_id: &str) -> &'a Value {
    wire.as_array()
        .expect("picker rows")
        .iter()
        .flat_map(|row| row["components"].as_array().into_iter().flatten())
        .find(|menu| menu["custom_id"] == custom_id)
        .unwrap_or_else(|| panic!("menu {custom_id} present in {wire}"))
}

fn placeholder_of(menu: &Value) -> &str {
    menu["placeholder"]
        .as_str()
        .expect("select menus carry a placeholder")
}

/// Fact A: the applying menu names its effect. Short copy is free, but the
/// words "apply" and "game" must both survive so members know which menu
/// changes their game roles.
fn assert_apply_placeholder(placeholder: &str) {
    let text = lower(placeholder);
    assert!(
        text.contains("apply"),
        "applying menu must name its apply effect, got {placeholder:?}"
    );
    assert!(
        text.contains("game"),
        "applying menu must name game roles, got {placeholder:?}"
    );
}

/// Fact C: the separate preview menu promises no changes. Short copy is free,
/// but "preview" plus an explicit only/no-change promise must both survive.
fn assert_preview_placeholder(placeholder: &str) {
    let text = lower(placeholder);
    assert!(
        text.contains("preview"),
        "preview menu must name preview, got {placeholder:?}"
    );
    assert!(
        [
            "only",
            "no change",
            "no role",
            "not change",
            "without",
            "no effect"
        ]
        .iter()
        .any(|word| text.contains(word)),
        "preview menu must promise only/no-change semantics, got {placeholder:?}"
    );
}

/// Facts A+B+C in the private picker caption: which menu applies, that
/// unselected catalog game roles are removed, that empty selection clears held
/// catalog game roles, and that preview changes nothing. Short accessible
/// language is free; the pinned words below are the contract. Selection itself
/// submits, so no submit-button language may appear.
fn assert_presubmit_caption(caption: &str) {
    let text = lower(caption);
    assert!(
        text.contains("apply"),
        "caption must name the applying menu, got {caption:?}"
    );
    assert!(
        text.contains("game"),
        "caption must name game roles, got {caption:?}"
    );
    assert!(
        text.contains("role"),
        "caption must name roles, got {caption:?}"
    );
    assert!(
        text.contains("remov"),
        "caption must say unselected catalog game roles are removed, got {caption:?}"
    );
    assert!(
        text.contains("clear"),
        "caption must say empty selection clears held catalog game roles, got {caption:?}"
    );
    assert!(
        [
            "empty",
            "nothing",
            "deselect",
            "unselect",
            "not select",
            "no game"
        ]
        .iter()
        .any(|word| text.contains(word)),
        "caption must name the empty/deselected case, got {caption:?}"
    );
    assert!(
        text.contains("preview"),
        "caption must point at the separate preview menu, got {caption:?}"
    );
    assert!(
        [
            "no change",
            "no role",
            "not change",
            "without changing",
            "won't change",
            "wont change",
            "only preview",
            "preview only",
            "preview-only",
            "no effect",
        ]
        .iter()
        .any(|word| text.contains(word)),
        "caption must promise preview changes nothing, got {caption:?}"
    );
    for banned in [
        "submit button",
        "click submit",
        "press submit",
        "then submit",
    ] {
        assert!(
            !text.contains(banned),
            "selection itself submits: no {banned:?} language, got {caption:?}"
        );
    }
}

#[test]
fn apply_menu_placeholder_names_apply_game_roles() {
    let wire = games_wire(&[]);
    let placeholder = placeholder_of(menu_by_id(&wire, GAME_SELECT_ID));
    assert_apply_placeholder(placeholder);
}

#[test]
fn preview_menu_placeholder_names_preview_only_no_role_changes() {
    let wire = games_wire(&[]);
    let placeholder = placeholder_of(menu_by_id(&wire, GAME_PREVIEW_SELECT_ID));
    assert_preview_placeholder(placeholder);
}

/// Mutant detector for Fact A: the old "What do you play?" question carries no
/// apply effect and must be gone.
#[test]
fn old_apply_placeholder_what_do_you_play_is_gone() {
    let wire = games_wire(&[]);
    let placeholder = placeholder_of(menu_by_id(&wire, GAME_SELECT_ID));
    assert_ne!(
        placeholder, OLD_APPLY_PLACEHOLDER,
        "old applying-menu copy still present"
    );
    assert!(
        !lower(placeholder).contains("what do you play"),
        "old applying-menu question still present, got {placeholder:?}"
    );
}

/// Mutant detector for Fact C: bare "proposed changes" without a no-change
/// promise must be gone.
#[test]
fn old_preview_placeholder_proposed_changes_is_gone() {
    let wire = games_wire(&[]);
    let placeholder = placeholder_of(menu_by_id(&wire, GAME_PREVIEW_SELECT_ID));
    assert_ne!(
        placeholder, OLD_PREVIEW_PLACEHOLDER,
        "old preview-menu copy still present"
    );
}

/// Control: component IDs, min/max values, selected defaults, role catalog,
/// option counts and placeholder bounds are unchanged. Only the two
/// placeholder strings may move.
#[test]
fn menu_identities_bounds_defaults_catalog_unchanged() {
    let shooters = pick_by_key("shooters").expect("catalog").role_id;
    let wire = games_wire(&[shooters]);
    let rows = wire.as_array().expect("two picker rows");
    assert_eq!(rows.len(), 2, "apply plus preview rows survive");
    let apply = menu_by_id(&wire, GAME_SELECT_ID);
    let preview = menu_by_id(&wire, GAME_PREVIEW_SELECT_ID);
    for (menu, id) in [(apply, GAME_SELECT_ID), (preview, GAME_PREVIEW_SELECT_ID)] {
        assert_eq!(menu["custom_id"], id);
        assert_eq!(menu["min_values"], 0, "empty stays allowed on {id}");
        assert_eq!(menu["max_values"], 10, "full catalog stays allowed on {id}");
        let options = menu["options"].as_array().expect("catalog options");
        assert_eq!(options.len(), 10, "game catalog size on {id}");
        let values: HashSet<&str> = options
            .iter()
            .map(|option| option["value"].as_str().expect("stable key"))
            .collect();
        assert_eq!(
            values,
            GAME_PICKS.iter().map(|pick| pick.key).collect(),
            "stable game keys on {id}"
        );
        let placeholder = placeholder_of(menu);
        assert!(
            !placeholder.trim().is_empty(),
            "placeholder stays set on {id}"
        );
        assert!(
            placeholder.encode_utf16().count() <= 150,
            "placeholder fits Discord limits on {id}"
        );
        assert!(
            !placeholder.contains("<@")
                && !placeholder.contains("@everyone")
                && !placeholder.contains("@here"),
            "placeholder carries no mention markup on {id}"
        );
    }
    let defaults: Vec<&str> = apply["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|option| option["default"] == true)
        .map(|option| option["value"].as_str().unwrap())
        .collect();
    assert_eq!(
        defaults,
        vec!["shooters"],
        "current game roles still preselect on the apply route"
    );
    assert!(
        preview["options"]
            .as_array()
            .unwrap()
            .iter()
            .all(|option| option["default"] == false),
        "preview still proposes, never confirms current roles"
    );
}

/// Control: actual apply/removal policy is unchanged. Empty still clears held
/// catalog game roles without recording; dry run still changes nothing.
#[test]
fn adjudicate_empty_clear_and_dry_run_unchanged() {
    let shooters = pick_by_key("shooters").expect("catalog").role_id;
    let cleared = adjudicate_game_select(
        OnboardingMode::Legacy,
        &[],
        &[shooters],
        &|_| true,
        22,
        false,
    )
    .expect("legacy serves the picker");
    assert_eq!(cleared.remove_role_ids, vec![shooters.to_owned()]);
    assert!(cleared.add_role_ids.is_empty());
    assert_eq!(cleared.reply, PICKER_CLEARED_REPLY);
    assert!(!cleared.record_selected && !cleared.record_routed);
    let dry = adjudicate_game_select(
        OnboardingMode::Legacy,
        &["shooters"],
        &[],
        &|_| true,
        22,
        true,
    )
    .expect("dry run still answers");
    assert_eq!(dry.reply, PICKER_DRY_RUN_REPLY);
    assert!(dry.add_role_ids.is_empty() && dry.remove_role_ids.is_empty());
    assert!(!dry.record_selected && !dry.record_routed);
}

// --- dispatched private-picker fixtures -------------------------------------

const CLARITY_BOT_ROLE: &str = "771";
const CLARITY_UNRELATED: &str = "770";

#[derive(Default)]
struct ClarityState {
    held: HashSet<String>,
}

async fn clarity_discord(state: Arc<Mutex<ClarityState>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let mut state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/guilds/22") => {
                ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"}))
            }
            ("GET", "/guilds/22/roles") => {
                let mut roles = vec![
                    json!({"id":"22","name":"@everyone","permissions":"1051648","position":0,"managed":false}),
                    json!({"id":CLARITY_UNRELATED,"name":"Unrelated","permissions":"0","position":1,"managed":false}),
                    json!({"id":CLARITY_BOT_ROLE,"name":"Bot","permissions":"268435456","position":100,"managed":false}),
                ];
                roles.extend(GAME_PICKS.iter().chain(PLATFORM_PICKS.iter()).map(
                    |pick| {
                        json!({"id":pick.role_id,"name":pick.label,"permissions":"0","position":10,"managed":false})
                    },
                ));
                ScriptedResponse::json(200, Value::Array(roles))
            }
            ("GET", path) if path.starts_with("/guilds/22/members/") => {
                let id = path.strip_prefix("/guilds/22/members/").unwrap();
                assert!(
                    matches!(id, "44" | "999"),
                    "picker reads only self and bot: {id}"
                );
                let roles = if id == "999" {
                    vec![CLARITY_BOT_ROLE.to_owned()]
                } else {
                    state.held.iter().cloned().collect::<Vec<_>>()
                };
                ScriptedResponse::json(200, json!({
                    "user":{"id":id,"bot":id == "999"},
                    "roles":roles,"pending":false
                }))
            }
            ("GET", path) if path.starts_with("/channels/") => {
                let id = path.strip_prefix("/channels/").unwrap();
                ScriptedResponse::json(200, json!({"id":id,"guild_id":"22","type":0,"permission_overwrites":[]}))
            }
            ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
            ("PATCH", path)
                if path.starts_with("/webhooks/111/")
                    && path.ends_with("/messages/@original") =>
            {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            ("PUT" | "DELETE", path)
                if path.starts_with("/guilds/22/members/44/roles/") =>
            {
                let role = path.rsplit('/').next().unwrap();
                if request.method == "PUT" {
                    state.held.insert(role.into());
                } else {
                    state.held.remove(role);
                }
                ScriptedResponse::status(204)
            }
            _ => ScriptedResponse::status(403),
        }
    })
    .await
}

fn clarity_invocation(data: Value, kind: u8, serial: u64) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"111","authorizing_integration_owners":{"0":"22"},
        "id":serial.to_string(),"token":format!("clarity-fixture-{serial}"),
        "type":kind,"version":1,"guild_id":"22",
        "member":{"permissions":"0","roles":[],"deaf":false,"mute":false,"flags":0,
            "user":{"id":"44","username":"member","discriminator":"0","bot":false}},
        "data":data
    }))
    .expect("synthetic picker interaction")
}

fn clarity_reopen_games(serial: u64) -> Interaction {
    clarity_invocation(
        json!({"id":"444","name":"reopen","type":1,"options":[{"name":"kind","type":3,"value":"games"}]}),
        2,
        serial,
    )
}

fn clarity_select(id: &str, keys: &[&str], serial: u64) -> Interaction {
    clarity_invocation(
        json!({"custom_id":id,"component_type":3,"values":keys}),
        3,
        serial,
    )
}

fn clarity_component_from_body(body: &Value, id: &str, keys: &[&str], serial: u64) -> Interaction {
    let mut wire = serde_json::to_value(clarity_select(id, keys, serial)).unwrap();
    wire["message"] = json!({
        "id":"98", "channel_id":"12", "guild_id":"22", "type":0,
        "author":{"id":"999","username":"fixture-bot","discriminator":"0","bot":true},
        "content":body["content"],"components":body["components"],"flags":64,
        "timestamp":"2026-10-25T00:00:00+00:00", "edited_timestamp":null,
        "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
        "attachments":[],"embeds":[],"pinned":false
    });
    serde_json::from_value(wire).expect("component carries its private bot message")
}

fn clarity_last_edit(mock: &MockRest) -> Value {
    let requests = mock.requests();
    let edit = requests
        .iter()
        .rev()
        .find(|request| request.method == "PATCH" && request.path.ends_with("/messages/@original"))
        .expect("picker finishes its private defer");
    serde_json::from_slice(&edit.body).expect("response JSON")
}

fn clarity_controls(body: &Value) -> Vec<&Value> {
    body["components"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|row| row["components"].as_array().into_iter().flatten())
        .collect()
}

fn clarity_content(body: &Value) -> &str {
    body["content"]
        .as_str()
        .expect("private human-readable feedback")
}

fn clarity_mentions_blocked(body: &Value) {
    assert_eq!(
        body["allowed_mentions"]["parse"],
        json!([]),
        "no implicit mentions: {body}"
    );
    for key in ["users", "roles"] {
        assert!(
            body["allowed_mentions"][key].is_null() || body["allowed_mentions"][key] == json!([]),
            "no explicit mention recipients: {body}"
        );
    }
    assert_ne!(body["allowed_mentions"]["replied_user"], true);
}

async fn clarity_submit(
    runtime: &OnboardingRuntime,
    mock: &MockRest,
    interaction: Interaction,
) -> Value {
    assert!(
        runtime.accepts_interaction(&interaction),
        "onboarding owns the picker interaction at ingress"
    );
    let start = mock.requests().len();
    runtime
        .handle(OnboardingJob::Interaction(Box::new(interaction)), NOW)
        .await
        .expect("private picker operation finishes");
    let requests = mock.requests();
    assert!(
        requests[start].path.ends_with("/callback"),
        "defer before any member/config read"
    );
    assert_eq!(
        requests[start..]
            .iter()
            .filter(|request| request.method == "POST" && request.path.ends_with("/callback"))
            .count(),
        1,
        "one ingress ACK, no double callback"
    );
    assert_eq!(
        requests[start..]
            .iter()
            .filter(|request| request.method == "PATCH")
            .count(),
        1,
        "finish the private defer exactly once"
    );
    clarity_last_edit(mock)
}

async fn clarity_event_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[derive(Clone, Copy)]
enum ClarityCase {
    Caption,
    OldCaption,
    PrivateBoundsNoMutations,
    PreviewNextAction,
    EmptyPreviewNoClear,
}

async fn run_clarity_case(case: ClarityCase) {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert_ne!(
            std::env::var("GITHUB_ACTIONS").as_deref(),
            Ok("true"),
            "CI must configure the guarded disposable database; missing configuration is not a passing acceptance run"
        );
        eprintln!("SKIP presubmit-clarity DB acceptance: set guarded TWO_TEST_DATABASE_URL; no behavior was verified");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    let result = AssertUnwindSafe(Box::pin(exercise_clarity_case(case, db.pool())))
        .catch_unwind()
        .await;
    db.close()
        .await
        .expect("verify disposable fixture teardown even on assertion failure");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn clarity_role_id(key: &str) -> String {
    GAME_PICKS
        .iter()
        .find(|pick| pick.key == key)
        .expect("catalog keeps the pick")
        .role_id
        .to_owned()
}

async fn exercise_clarity_case(case: ClarityCase, pool: &PgPool) {
    let state = Arc::new(Mutex::new(ClarityState {
        held: [
            clarity_role_id("shooters"),
            clarity_role_id("survival"),
            CLARITY_UNRELATED.into(),
        ]
        .into(),
    }));
    let mock = clarity_discord(state.clone()).await;
    let runtime = runtime(pool, &mock, &vars("legacy", false));
    let body = clarity_submit(&runtime, &mock, clarity_reopen_games(600)).await;
    match case {
        ClarityCase::Caption => {
            let caption = clarity_content(&body);
            assert_presubmit_caption(caption);
            let controls = clarity_controls(&body);
            let apply = controls
                .iter()
                .find(|control| control["custom_id"] == GAME_SELECT_ID)
                .expect("dispatched apply menu");
            let preview = controls
                .iter()
                .find(|control| control["custom_id"] == GAME_PREVIEW_SELECT_ID)
                .expect("dispatched preview menu");
            assert_apply_placeholder(apply["placeholder"].as_str().expect("apply placeholder"));
            assert_preview_placeholder(
                preview["placeholder"]
                    .as_str()
                    .expect("preview placeholder"),
            );
        }
        ClarityCase::OldCaption => {
            let caption = clarity_content(&body);
            assert_ne!(
                caption, OLD_GAMES_CAPTION,
                "old picker caption still present"
            );
            assert!(
                !lower(caption).contains("pick what you play, then submit"),
                "old pick-then-submit sentence still present, got {caption:?}"
            );
        }
        ClarityCase::PrivateBoundsNoMutations => {
            let requests = mock.requests();
            let defer: Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(defer["type"], 5);
            assert_eq!(defer["data"]["flags"], 64, "picker stays self-only");
            clarity_mentions_blocked(&body);
            let caption = clarity_content(&body);
            assert!(!caption.trim().is_empty(), "menu carries a caption");
            assert!(
                caption.encode_utf16().count() <= 2000,
                "caption fits Discord limits"
            );
            assert!(
                !caption.contains("<@")
                    && !caption.contains("@everyone")
                    && !caption.contains("@here"),
                "caption carries no mention markup: {caption:?}"
            );
            for control in clarity_controls(&body) {
                let placeholder = control["placeholder"].as_str().expect("select placeholder");
                assert!(placeholder.encode_utf16().count() <= 150);
            }
            assert!(
                mock.requests().iter().all(|request| !(matches!(
                    request.method.as_str(),
                    "PUT" | "DELETE"
                ) && request.path.contains("/roles/"))),
                "opening a menu grants/removes no roles"
            );
            assert!(
                posts(&mock).is_empty(),
                "opening a menu posts no welcome/farewell"
            );
            assert_eq!(
                clarity_event_count(pool).await,
                0,
                "opening a menu records no funnel/selection events"
            );
            assert!(
                !has_onboarding_prompt(pool, "22", "44").await.unwrap(),
                "opening a menu arms no prompt guard"
            );
        }
        ClarityCase::PreviewNextAction => {
            let preview_id = clarity_controls(&body)
                .into_iter()
                .find(|control| control["custom_id"] == GAME_PREVIEW_SELECT_ID)
                .expect("dispatched preview menu")["custom_id"]
                .as_str()
                .unwrap()
                .to_owned();
            let before = state.lock().unwrap().held.clone();
            let result = clarity_submit(
                &runtime,
                &mock,
                clarity_component_from_body(&body, &preview_id, &[], 601),
            )
            .await;
            let text = clarity_content(&result);
            assert!(
                text.contains("Preview — roles not changed"),
                "preview keeps its no-effect label: {text:?}"
            );
            assert!(
                text.contains(PREVIEW_NEXT_ACTION),
                "preview keeps its next-action guidance: {text:?}"
            );
            for heading in ["Additions", "Removals", "Unchanged"] {
                assert!(
                    text.contains(heading),
                    "preview keeps its {heading} section: {text:?}"
                );
            }
            assert_eq!(
                state.lock().unwrap().held,
                before,
                "preview changes no roles"
            );
            assert_eq!(
                clarity_event_count(pool).await,
                0,
                "preview records no funnel/selection events"
            );
        }
        ClarityCase::EmptyPreviewNoClear => {
            let preview_id = clarity_controls(&body)
                .into_iter()
                .find(|control| control["custom_id"] == GAME_PREVIEW_SELECT_ID)
                .expect("dispatched preview menu")["custom_id"]
                .as_str()
                .unwrap()
                .to_owned();
            let result = clarity_submit(
                &runtime,
                &mock,
                clarity_component_from_body(&body, &preview_id, &[], 602),
            )
            .await;
            let text = clarity_content(&result);
            assert!(
                text.contains("Shooters") && text.contains("Survival"),
                "empty preview lists held games as removals: {text:?}"
            );
            assert!(
                !text.contains("Cleared your game roles"),
                "empty preview is not an apply: {text:?}"
            );
            assert!(
                !text.contains("Game roles saved.") && !text.contains("Done. Here is where to go:"),
                "preview borrows no apply success copy: {text:?}"
            );
            assert_eq!(
                state.lock().unwrap().held,
                HashSet::from([
                    clarity_role_id("shooters"),
                    clarity_role_id("survival"),
                    CLARITY_UNRELATED.into(),
                ]),
                "empty preview clears nothing"
            );
            assert_eq!(clarity_event_count(pool).await, 0);
        }
    }
}

#[tokio::test]
async fn reopen_caption_explains_apply_clear_and_preview_before_submission() {
    run_clarity_case(ClarityCase::Caption).await;
}

#[tokio::test]
async fn old_caption_pick_then_submit_is_gone() {
    run_clarity_case(ClarityCase::OldCaption).await;
}

#[tokio::test]
async fn reopen_private_bounded_mention_safe_zero_mutations() {
    run_clarity_case(ClarityCase::PrivateBoundsNoMutations).await;
}

#[tokio::test]
async fn preview_result_next_action_text_unchanged() {
    run_clarity_case(ClarityCase::PreviewNextAction).await;
}

#[tokio::test]
async fn preview_empty_lists_removals_without_clearing() {
    run_clarity_case(ClarityCase::EmptyPreviewNoClear).await;
}
