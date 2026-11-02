//! Acceptance: picker replies reflect committed state, and a rejoin re-welcomes.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the RED tests pass by changing
//! production code only (`crates/bot/src/onboarding.rs`,
//! `crates/core/src/onboarding_store.rs`, and the `Goodbye` job plumbing its
//! clearing path needs). Do not edit the fixtures or assertions: they are the
//! spec.
//!
//! Three durable-truth gaps:
//!
//! 1. Both picker branches reply BEFORE committing: the session branch stages
//!    the route, edits the success reply, then commits; the game branch
//!    records, replies, then commits. A commit failure therefore leaves a
//!    false routed/saved claim on the member's screen (followed by a second,
//!    corrective failure edit). The fix commits before replying, so a commit
//!    failure yields exactly one reply: the failure path, never a success
//!    claim.
//!
//! 2. The game role loop returns on the first failed role write after earlier
//!    roles already flipped, and the failure reply names neither the applied
//!    nor the pending roles. The fix tracks which roles flipped, names
//!    applied vs pending in the failure reply, and lets a retry converge to
//!    full success instead of erroring on already-applied roles. The durable
//!    recording shape for partial progress is the builder's choice: these
//!    tests pin the reply and the convergence, not the row shape.
//!
//! 3. The once-per-member prompt guard has no clearing path: a leave posts a
//!    goodbye but a rejoin is never welcomed again. The fix clears the guard
//!    when the goodbye job completes for that member, so the next join
//!    re-welcomes; privacy suppression is re-checked, so suppressed members
//!    stay silent.
//!
//! RED at baseline (5 tests): both commit-failure tests (two replies, the
//! first a success claim), the partial-role naming test (generic reply), and
//! the goodbye/rejoin tests (guard never clears, no second welcome).
//!
//! Controls (4 tests): both picker success paths, suppressed-member silence
//! across goodbye and rejoin, and the offline success-marker vocabulary.
//!
//! This file brings its own mock Discord responder so fixture changes in the
//! sibling runtime tests cannot change what is pinned here. It uses the
//! sibling `TestSchema`, `runtime`, `vars`, `component`, `welcome`,
//! `replies`, `posts` and `has_onboarding_prompt` helpers verbatim; the
//! builder must keep those signatures stable. Goodbye jobs are built through
//! the production `capture` path (not struct literals) so `Goodbye` can gain
//! the member identity its clearing path needs without touching this file.
//!
//! Disposable-DB only (plus one offline test): no live Discord, no
//! staging/production database, never `DATABASE_URL`.

use super::*;

#[derive(Default)]
struct DurableDiscordState {
    /// Roles the double has granted (shared across test members, as in the
    /// sibling fixture: per-member role sets are not what is pinned here).
    roles: HashSet<String>,
    /// Role ids whose grant/revoke answers 403, for partial-failure tests.
    fail_roles: HashSet<String>,
}

async fn durable_discord(state: Arc<Mutex<DurableDiscordState>>) -> MockRest {
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
                if state.fail_roles.contains(&role) {
                    return ScriptedResponse::status(403);
                }
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
            ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
            ("PATCH", path) if path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

/// Fail the picker's COMMIT while letting its staged inserts and Discord
/// writes proceed: a deferred constraint trigger fires at commit, after the
/// reply path has run. A reply-before-commit implementation then shows two
/// replies (success claim plus correction); commit-before-reply shows one
/// failure reply. Schema-local: the trigger lives in the test's own schema
/// and vanishes with it.
async fn force_commit_failure(db: &TestSchema) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION tmp_fail_onboarding_commit() RETURNS trigger \
         LANGUAGE plpgsql AS $$ BEGIN \
         RAISE EXCEPTION 'forced commit failure for durable-picker acceptance'; END; $$",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE CONSTRAINT TRIGGER tmp_fail_onboarding_commit_trigger \
         AFTER INSERT ON events DEFERRABLE INITIALLY DEFERRED \
         FOR EACH ROW EXECUTE FUNCTION tmp_fail_onboarding_commit()",
    )
    .execute(&db.pool)
    .await
    .unwrap();
}

/// Build a goodbye job through the production leave-capture path, so this
/// frozen file never names `Goodbye`'s fields: the clearing fix may add the
/// member identity the job needs without touching these tests.
fn goodbye_for_leave(runtime: &OnboardingRuntime, member_id: u64) -> OnboardingJob {
    let pipeline = build_pipeline(vec![], None);
    let remove = Event::MemberRemove(
        serde_json::from_value(json!({
            "guild_id": "22",
            "user": {"id": member_id.to_string(), "username": "member", "discriminator": "0"},
        }))
        .unwrap(),
    );
    let job = runtime
        .capture(&remove, &pipeline)
        .expect("a leave captures a goodbye job");
    assert!(matches!(job, OnboardingJob::Goodbye { .. }));
    job
}

async fn suppress(db: &TestSchema, member_id: &str) {
    sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
        .bind("22")
        .bind(member_id)
        .execute(&db.pool)
        .await
        .unwrap();
}

fn reply_contents(mock: &MockRest) -> Vec<String> {
    replies(mock)
        .iter()
        .map(|reply| {
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            body["content"].as_str().unwrap().to_owned()
        })
        .collect()
}

fn role_id_for(key: &str) -> String {
    GAME_PICKS
        .iter()
        .find(|pick| pick.key == key)
        .unwrap_or_else(|| panic!("catalog keeps the {key} pick"))
        .role_id
        .to_owned()
}

// RED at baseline: the session branch stages the route, edits the success
// reply, then commits, so a commit failure shows success-then-correction.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_session_commit_failure_yields_failure_reply_with_no_routed_claim() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        force_commit_failure(&db).await;
        let mock = durable_discord(Arc::new(Mutex::new(DurableDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime
            .handle(component(SESSION_SELECT_ID, &["find-players"]), NOW)
            .await
            .unwrap();
        let requests = mock.requests();
        assert_eq!(
            requests[0].method, "POST",
            "the interaction is still deferred once"
        );
        assert!(
            requests[0].path.ends_with("/callback"),
            "first request is the defer"
        );
        let contents = reply_contents(&mock);
        assert_eq!(
            contents.len(),
            1,
            "one reply only: the failure path, never success-then-correction, got {contents:?}"
        );
        assert!(
            contents[0].contains("couldn't finish routing"),
            "a commit failure yields the failure reply, got {:?}",
            contents[0]
        );
        assert!(
            !contents[0].contains("On it"),
            "no routed claim on commit failure, got {:?}",
            contents[0]
        );
        assert!(
            !contents[0].contains("<#"),
            "no channel link on commit failure, got {:?}",
            contents[0]
        );
        assert_eq!(
            db.count(EVENT_CHANNEL_ROUTED).await,
            0,
            "the failed commit records nothing"
        );
        assert_eq!(db.count(EVENT_ONBOARDING_PROMPTED).await, 0);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: the game branch records, replies, then commits, so a
// commit failure shows the saved claim before the correction.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_game_commit_failure_yields_failure_reply_with_no_saved_claim() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        force_commit_failure(&db).await;
        let mock = durable_discord(Arc::new(Mutex::new(DurableDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        runtime
            .handle(component(GAME_SELECT_ID, &["shooters"]), NOW)
            .await
            .unwrap();
        let requests = mock.requests();
        assert_eq!(
            requests[0].method, "POST",
            "the interaction is still deferred once"
        );
        assert!(
            requests[0].path.ends_with("/callback"),
            "first request is the defer"
        );
        let contents = reply_contents(&mock);
        assert_eq!(
            contents.len(),
            1,
            "one reply only: the failure path, never success-then-correction, got {contents:?}"
        );
        assert!(
            contents[0].contains("couldn't finish"),
            "a commit failure yields the failure reply, got {:?}",
            contents[0]
        );
        assert!(
            !contents[0].contains("Done."),
            "no saved claim on commit failure, got {:?}",
            contents[0]
        );
        assert!(
            !contents[0].contains("Game roles saved."),
            "no saved claim on commit failure, got {:?}",
            contents[0]
        );
        assert_eq!(
            db.count(EVENT_GAME_ROLES_SELECTED).await,
            0,
            "the failed commit records nothing"
        );
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 0);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: the first failed role write aborts the loop with the
// generic reply, which names neither the applied nor the pending roles. The
// retry half is the convergence contract: once the pending role grants, the
// same submission succeeds instead of erroring on the already-applied role.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_game_partial_role_failure_names_applied_vs_pending_and_retry_converges() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let shooters = role_id_for("shooters");
        let survival = role_id_for("survival");
        let state = Arc::new(Mutex::new(DurableDiscordState {
            roles: HashSet::new(),
            fail_roles: HashSet::from([survival.clone()]),
        }));
        let mock = durable_discord(Arc::clone(&state)).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        runtime
            .handle(component(GAME_SELECT_ID, &["shooters", "survival"]), NOW)
            .await
            .unwrap();
        let contents = reply_contents(&mock);
        assert_eq!(contents.len(), 1, "one failure reply, got {contents:?}");
        assert!(
            contents[0].contains("shooters") || contents[0].contains("Shooter"),
            "the failure reply names the applied pick, got {:?}",
            contents[0]
        );
        assert!(
            contents[0].contains("survival") || contents[0].contains("Survival"),
            "the failure reply names the pending pick, got {:?}",
            contents[0]
        );
        assert_ne!(
            contents[0], PICKER_ROLE_FAILURE_REPLY,
            "a partial failure gets its own copy, not the generic role reply"
        );
        assert!(
            state.lock().unwrap().roles.contains(&shooters),
            "the applied role effect landed before the failure"
        );
        assert!(
            !state.lock().unwrap().roles.contains(&survival),
            "the pending role was not granted"
        );
        // The pending role becomes grantable; the member retries the same
        // whole-answer submission and converges to full success.
        state.lock().unwrap().fail_roles.clear();
        runtime
            .handle(
                component(GAME_SELECT_ID, &["shooters", "survival"]),
                NOW + 1,
            )
            .await
            .unwrap();
        let contents = reply_contents(&mock);
        assert_eq!(contents.len(), 2, "failure then success, got {contents:?}");
        assert!(
            contents[1].contains("Done."),
            "the retry converges to the success reply, got {:?}",
            contents[1]
        );
        assert!(
            !contents[1].contains("couldn't"),
            "the retry carries no failure claim, got {:?}",
            contents[1]
        );
        assert!(
            state.lock().unwrap().roles.contains(&shooters)
                && state.lock().unwrap().roles.contains(&survival),
            "both roles granted after the retry"
        );
        assert!(
            db.count(EVENT_GAME_ROLES_SELECTED).await >= 1,
            "the converged retry records the selection"
        );
        assert!(
            db.count(EVENT_CHANNEL_ROUTED).await >= 1,
            "the converged retry records the route"
        );
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: the prompt guard has no clearing path, so it survives the
// goodbye that should release it.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_goodbye_clears_the_prompt_guard() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = durable_discord(Arc::new(Mutex::new(DurableDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime.handle(welcome(44), NOW).await.unwrap();
        assert_eq!(posts(&mock).len(), 1, "the welcome posted");
        assert!(
            has_onboarding_prompt(&db.pool, "22", "44").await.unwrap(),
            "the welcome armed the prompt guard"
        );
        let goodbye = goodbye_for_leave(&runtime, 44);
        runtime.handle(goodbye, NOW + 1).await.unwrap();
        assert_eq!(posts(&mock).len(), 2, "the goodbye post completed");
        assert!(
            !has_onboarding_prompt(&db.pool, "22", "44").await.unwrap(),
            "a completed goodbye clears the prompt guard"
        );
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// RED at baseline: the surviving guard swallows the rejoin welcome.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_rejoin_after_goodbye_rewelcomes() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = durable_discord(Arc::new(Mutex::new(DurableDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime.handle(welcome(44), NOW).await.unwrap();
        assert_eq!(posts(&mock).len(), 1, "the welcome posted");
        let goodbye = goodbye_for_leave(&runtime, 44);
        runtime.handle(goodbye, NOW + 1).await.unwrap();
        assert_eq!(posts(&mock).len(), 2, "the goodbye post completed");
        runtime.handle(welcome(44), NOW + 2).await.unwrap();
        assert_eq!(
            posts(&mock).len(),
            3,
            "a rejoin after a completed goodbye is welcomed again"
        );
        assert!(
            has_onboarding_prompt(&db.pool, "22", "44").await.unwrap(),
            "the re-welcome re-arms the prompt guard"
        );
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Control: the session success path keeps routing exactly once.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_session_picker_success_still_routes_control() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = durable_discord(Arc::new(Mutex::new(DurableDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime
            .handle(component(SESSION_SELECT_ID, &["find-players"]), NOW)
            .await
            .unwrap();
        let requests = mock.requests();
        assert_eq!(
            requests[0].method, "POST",
            "the interaction is deferred once"
        );
        assert!(
            requests[0].path.ends_with("/callback"),
            "first request is the defer"
        );
        let contents = reply_contents(&mock);
        assert_eq!(contents, vec!["On it - head to <#10>.".to_owned()]);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Control: the game success path keeps granting and recording exactly once.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_game_picker_success_still_grants_and_records_control() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let state = Arc::new(Mutex::new(DurableDiscordState::default()));
        let mock = durable_discord(Arc::clone(&state)).await;
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
            contents[0].contains("Shooters"),
            "the success reply names the pick, got {:?}",
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

// Control: suppression survives the new clearing path. A suppressed member is
// never welcomed, and goodbye plus rejoin does not resurrect the welcome.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_suppressed_member_stays_silent_across_goodbye_and_rejoin_control() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        suppress(&db, "45").await;
        let mock = durable_discord(Arc::new(Mutex::new(DurableDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("session", false));
        runtime.handle(welcome(45), NOW).await.unwrap();
        assert_eq!(posts(&mock).len(), 0, "no welcome for a suppressed member");
        assert!(
            !has_onboarding_prompt(&db.pool, "22", "45").await.unwrap(),
            "no prompt row for a suppressed member"
        );
        let goodbye = goodbye_for_leave(&runtime, 45);
        runtime.handle(goodbye, NOW + 1).await.unwrap();
        let after_goodbye = posts(&mock).len();
        runtime.handle(welcome(45), NOW + 2).await.unwrap();
        assert_eq!(
            posts(&mock).len(),
            after_goodbye,
            "still no welcome after goodbye plus rejoin"
        );
        assert!(
            !has_onboarding_prompt(&db.pool, "22", "45").await.unwrap(),
            "still no prompt row after goodbye plus rejoin"
        );
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Control (offline): the success markers the commit-failure tests treat as
// claims really are what the success builders emit, so "no success claim"
// means the success path did not run.
#[test]
fn onboarding_picker_success_markers_shape_the_no_false_claim_assertions() {
    let catalog = build_session_picks("10", "11");
    let routed = plan_session(&["find-players"], &|_| true, &catalog);
    assert!(session_ack_text(&routed).contains("<#10>"));
    let plan = plan_game_selection(&["shooters"], &|_| true);
    assert!(game_picker_reply(&plan, 22).starts_with("Done."));
    let dark = plan_game_selection(&["shooters"], &|_| false);
    assert!(game_picker_reply(&dark, 22).starts_with("Game roles saved."));
}
