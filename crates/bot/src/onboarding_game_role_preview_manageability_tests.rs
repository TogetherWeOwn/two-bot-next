//! Builder-owned regression pins for the game-role preview manageability
//! guard. The protected acceptance corpus proves an unmanageable ADDITION is
//! refused; these cases pin the two behaviours it cannot see:
//!
//! - the guard also covers REMOVALS (a held game role that became managed or
//!   moved at/above the bot must not be previewed as removable), and
//! - the hierarchy comparison is `>=`: a game role exactly at the bot's
//!   highest position is not manageable, matching the house hierarchy oracle.
//!
//! Dropping `.chain(removals.iter())` from the guard loop or weakening `>=`
//! to `>` must turn this module red.
#![cfg(test)]

use super::*;
use futures_util::FutureExt as _;
use std::panic::AssertUnwindSafe;
use two_bot_testsupport::TestDatabase;

const UNRELATED: &str = "770";
const BOT_ROLE: &str = "771";
const BOT_HIGHEST_POSITION: i64 = 100;

#[derive(Default)]
struct GuardState {
    held: HashSet<String>,
    managed_role: Option<String>,
    positions: HashMap<String, i64>,
}

async fn guard_discord(state: Arc<Mutex<GuardState>>) -> MockRest {
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
                    json!({"id":UNRELATED,"name":"Unrelated","permissions":"0","position":1,"managed":false}),
                    json!({"id":BOT_ROLE,"name":"Bot","permissions":"268435456","position":BOT_HIGHEST_POSITION,"managed":false}),
                ];
                roles.extend(GAME_PICKS.iter().chain(PLATFORM_PICKS.iter()).map(|pick| {
                    json!({
                        "id":pick.role_id,"name":pick.label,"permissions":"0",
                        "position":state.positions.get(pick.role_id).copied().unwrap_or(10),
                        "managed":state.managed_role.as_deref() == Some(pick.role_id)
                    })
                }));
                ScriptedResponse::json(200, Value::Array(roles))
            }
            ("GET", path) if path.starts_with("/guilds/22/members/") => {
                let id = path.strip_prefix("/guilds/22/members/").unwrap();
                assert!(matches!(id, "44" | "999"), "preview reads only self and bot: {id}");
                let roles = if id == "999" {
                    vec![BOT_ROLE.to_owned()]
                } else {
                    state.held.iter().cloned().collect::<Vec<_>>()
                };
                ScriptedResponse::json(
                    200,
                    json!({"user":{"id":id,"bot":id == "999"},"roles":roles,"pending":false}),
                )
            }
            ("GET", path) if path.starts_with("/channels/") => {
                let id = path.strip_prefix("/channels/").unwrap();
                ScriptedResponse::json(
                    200,
                    json!({"id":id,"guild_id":"22","type":0,"permission_overwrites":[]}),
                )
            }
            ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
            ("PATCH", path)
                if path.starts_with("/webhooks/111/") && path.ends_with("/messages/@original") =>
            {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            // Observable so the no-write assertions below can reject them.
            ("PUT" | "DELETE", path) if path.starts_with("/guilds/22/members/44/roles/") => {
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

/// Submit one preview through the real worker and return the private reply
/// text, proving the submission wrote no roles and changed no membership.
async fn submit_preview(
    runtime: &OnboardingRuntime,
    mock: &MockRest,
    state: &Arc<Mutex<GuardState>>,
    keys: &[&str],
) -> String {
    let held_before = state.lock().unwrap().held.clone();
    let start = mock.requests().len();
    runtime
        .handle(component(GAME_PREVIEW_SELECT_ID, keys), NOW)
        .await
        .unwrap();
    let requests = mock.requests();
    assert!(
        requests[start..]
            .iter()
            .all(|request| !matches!(request.method.as_str(), "PUT" | "DELETE")),
        "preview must not write roles"
    );
    assert_eq!(
        state.lock().unwrap().held,
        held_before,
        "preview applies nothing"
    );
    let edit = requests[start..]
        .iter()
        .rev()
        .find(|request| request.method == "PATCH")
        .expect("preview finishes its private defer");
    let body: Value = serde_json::from_slice(&edit.body).unwrap();
    body["content"]
        .as_str()
        .expect("private preview reply")
        .to_owned()
}

fn assert_disallowed(content: &str) {
    assert!(content.contains("Preview — roles not changed"), "{content}");
    assert!(
        content.contains("missing Manage Roles permission or hierarchy"),
        "expected the disallowed guidance, got: {content}"
    );
    assert!(
        !content.contains("**Removals**"),
        "a disallowed preview must not render a promising diff: {content}"
    );
}

fn assert_diff(content: &str) {
    assert!(content.contains("**Additions**"), "{content}");
    assert!(content.contains("**Removals**"), "{content}");
    assert!(
        !content.contains("missing Manage Roles permission or hierarchy"),
        "a manageable preview must not be refused: {content}"
    );
}

#[derive(Clone, Copy)]
enum GuardCase {
    Removal,
    Equality,
}

async fn run_case(case: GuardCase) {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert_ne!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"), "CI must configure the guarded disposable database; missing configuration is not a passing run");
        eprintln!("SKIP game-role preview manageability pins: set guarded TWO_TEST_DATABASE_URL; no behavior was verified");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    let result = AssertUnwindSafe(Box::pin(exercise(case, db.pool())))
        .catch_unwind()
        .await;
    db.close()
        .await
        .expect("verify disposable fixture teardown even on assertion failure");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn exercise(case: GuardCase, pool: &PgPool) {
    let state = Arc::new(Mutex::new(GuardState {
        held: [
            role_id_of("shooters"),
            role_id_of("survival"),
            UNRELATED.into(),
        ]
        .into(),
        ..GuardState::default()
    }));
    let mock = guard_discord(state.clone()).await;
    let runtime = runtime(pool, &mock, &vars("legacy", false));
    match case {
        GuardCase::Removal => {
            // A HELD role turned managed: deselecting it is a removal the bot
            // cannot perform, so the preview must refuse, not list it under
            // Removals. An additions-only guard loop renders the diff here.
            state.lock().unwrap().managed_role = Some(role_id_of("shooters"));
            let content = submit_preview(&runtime, &mock, &state, &["survival"]).await;
            assert_disallowed(&content);
            // A HELD role moved above the bot: an empty proposal removes it.
            {
                let mut state = state.lock().unwrap();
                state.managed_role = None;
                state.positions.insert(role_id_of("shooters"), 200);
            }
            let content = submit_preview(&runtime, &mock, &state, &[]).await;
            assert_disallowed(&content);
            // Control: the same removal previews fine once manageable again.
            state.lock().unwrap().positions.clear();
            let content = submit_preview(&runtime, &mock, &state, &["survival"]).await;
            assert_diff(&content);
            assert!(
                content.contains("**Removals**\nShooters"),
                "the control removal is previewed, not refused: {content}"
            );
        }
        GuardCase::Equality => {
            // Addition exactly at the bot's highest position: `>=` refuses,
            // a `>` mutant would preview Horror as an applyable addition.
            state
                .lock()
                .unwrap()
                .positions
                .insert(role_id_of("horror"), BOT_HIGHEST_POSITION);
            let content = submit_preview(&runtime, &mock, &state, &["horror", "survival"]).await;
            assert_disallowed(&content);
            // Removal exactly at the bot's highest position: same boundary on
            // the removals half of the loop.
            {
                let mut state = state.lock().unwrap();
                state.positions.clear();
                state
                    .positions
                    .insert(role_id_of("shooters"), BOT_HIGHEST_POSITION);
            }
            let content = submit_preview(&runtime, &mock, &state, &["survival"]).await;
            assert_disallowed(&content);
            // Control just below the boundary: position 99 is manageable.
            {
                let mut state = state.lock().unwrap();
                state.positions.clear();
                state
                    .positions
                    .insert(role_id_of("horror"), BOT_HIGHEST_POSITION - 1);
            }
            let content =
                submit_preview(&runtime, &mock, &state, &["horror", "shooters", "survival"]).await;
            assert_diff(&content);
            assert!(
                content.contains("**Additions**\nHorror"),
                "a just-below-boundary addition is previewed, not refused: {content}"
            );
        }
    }
}

#[tokio::test]
async fn preview_refuses_unmanageable_removals_not_only_additions() {
    run_case(GuardCase::Removal).await;
}

#[tokio::test]
async fn preview_refuses_roles_exactly_at_the_bot_highest_position() {
    run_case(GuardCase::Equality).await;
}
