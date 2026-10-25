//! Protected acceptance: self-only proposed game choices are not applied choices.
//! Exercise the real onboarding ingress/worker and pure policy, never a preview stub.
//! A builder consumes this file unchanged; test corrections belong to its author.
//!
//! The private /reopen games response exposes one clearly named Preview control:
//! either a preview select, or a button opening a separate preview select. This
//! corpus discovers its ID from the wire, so no new command or hard-coded ID is
//! prescribed. The preview select submits stable game keys, not raw role IDs.
//! Preview output uses Additions / Removals / Unchanged sections and the required
//! "Preview — roles not changed" label. Other copy and implementation are free.
#![cfg(test)]

use super::*;
use crate::hotpath_settings_test_support::queries::Queries;
use futures_util::FutureExt as _;
use std::panic::AssertUnwindSafe;
use two_bot_testsupport::TestDatabase;

const UNRELATED: &str = "770";
const BOT_ROLE: &str = "771";

#[derive(Default)]
struct PreviewState {
    held: HashSet<String>,
    missing_role: Option<String>,
    managed_role: Option<String>,
    high_role: Option<String>,
    member_status: Option<u16>,
    wrong_member: bool,
    malformed_member: bool,
    roles_status: Option<u16>,
    reject_callback: bool,
    bot_cannot_manage: bool,
}

async fn preview_discord(state: Arc<Mutex<PreviewState>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let mut state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/guilds/22") => ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"})),
            ("GET", "/guilds/22/roles") => {
                if let Some(status) = state.roles_status {
                    return ScriptedResponse::status(status);
                }
                let mut roles = vec![
                    json!({"id":"22","name":"@everyone","permissions":"1051648","position":0,"managed":false}),
                    json!({"id":UNRELATED,"name":"Unrelated","permissions":"0","position":1,"managed":false}),
                    json!({"id":BOT_ROLE,"name":"Bot","permissions":if state.bot_cannot_manage {"0"} else {"268435456"},"position":100,"managed":false}),
                ];
                roles.extend(GAME_PICKS.iter().chain(PLATFORM_PICKS.iter())
                    .filter(|pick| state.missing_role.as_deref() != Some(pick.role_id))
                    .map(|pick| json!({
                        "id":pick.role_id,"name":pick.label,"permissions":"0",
                        "position":if state.high_role.as_deref() == Some(pick.role_id) {200} else {10},
                        "managed":state.managed_role.as_deref() == Some(pick.role_id)
                    })));
                ScriptedResponse::json(200, Value::Array(roles))
            }
            ("GET", path) if path.starts_with("/guilds/22/members/") => {
                let id = path.strip_prefix("/guilds/22/members/").unwrap();
                assert!(matches!(id, "44" | "999"), "preview must read only self and bot: {id}");
                if id == "44" {
                    if let Some(status) = state.member_status {
                        return ScriptedResponse::status(status);
                    }
                    if state.malformed_member {
                        return ScriptedResponse::json(200, json!({"user":{"id":"44"},"roles":"not-an-array"}));
                    }
                }
                let roles = if id == "999" {vec![BOT_ROLE.to_owned()]} else {state.held.iter().cloned().collect::<Vec<_>>()};
                ScriptedResponse::json(200, json!({
                    "user":{"id":if id == "44" && state.wrong_member {"45"} else {id},"bot":id == "999"},
                    "roles":roles,"pending":false
                }))
            }
            ("GET", path) if path.starts_with("/channels/") => {
                let id = path.strip_prefix("/channels/").unwrap();
                ScriptedResponse::json(200, json!({"id":id,"guild_id":"22","type":0,"permission_overwrites":[]}))
            }
            ("POST", path) if path.ends_with("/callback") => {
                ScriptedResponse::status(if state.reject_callback {403} else {204})
            }
            ("PATCH", path) if path.starts_with("/webhooks/111/") && path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            // Keep the real apply control observable; preview assertions below
            // reject these requests even when a production path catches errors.
            ("PUT" | "DELETE", path) if path.starts_with("/guilds/22/members/44/roles/") => {
                let role = path.rsplit('/').next().unwrap();
                if request.method == "PUT" {state.held.insert(role.into());} else {state.held.remove(role);}
                ScriptedResponse::status(204)
            }
            _ => ScriptedResponse::status(403),
        }
    }).await
}

fn invocation(data: Value, kind: u8, serial: u64) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"111","authorizing_integration_owners":{"0":"22"},
        "id":serial.to_string(),"token":format!("preview-fixture-{serial}"),
        "type":kind,"version":1,"guild_id":"22",
        // Deliberately stale: the fresh REST response, not this payload, wins.
        "member":{"permissions":"0","roles":[],"deaf":false,"mute":false,"flags":0,
            "user":{"id":"44","username":"@everyone <@&770>","discriminator":"0","bot":false}},
        "data":data
    }))
    .expect("synthetic onboarding interaction")
}

fn reopen_games(serial: u64) -> Interaction {
    invocation(
        json!({"id":"444","name":"reopen","type":1,"options":[{"name":"kind","type":3,"value":"games"}]}),
        2,
        serial,
    )
}

fn select(id: &str, kind: u64, keys: &[&str], serial: u64) -> Interaction {
    invocation(
        json!({"custom_id":id,"component_type":kind,"values":keys}),
        3,
        serial,
    )
}

struct PreviewMenu {
    id: String,
    body: Value,
}

fn component_from_body(
    body: &Value,
    id: &str,
    kind: u64,
    keys: &[&str],
    serial: u64,
) -> Interaction {
    let mut wire = serde_json::to_value(select(id, kind, keys, serial)).unwrap();
    wire["message"] = json!({
        "id":"98", "channel_id":"12", "guild_id":"22", "type":0,
        "author":{"id":"999","username":"fixture-bot","discriminator":"0","bot":true},
        "content":body["content"],"components":body["components"],"flags":64,
        "timestamp":"2026-10-25T00:00:00+00:00", "edited_timestamp":null,
        "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
        "attachments":[],"embeds":[],"pinned":false
    });
    serde_json::from_value(wire).expect("component carries its originating private bot message")
}

fn preview_select(menu: &PreviewMenu, kind: u64, keys: &[&str], serial: u64) -> Interaction {
    component_from_body(&menu.body, &menu.id, kind, keys, serial)
}

fn last_edit(mock: &MockRest) -> Value {
    let requests = mock.requests();
    let edit = requests
        .iter()
        .rev()
        .find(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
        .expect("onboarding must finish its private defer");
    serde_json::from_slice(&edit.body).expect("response JSON")
}

fn controls(body: &Value) -> Vec<&Value> {
    body["components"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|row| row["components"].as_array().into_iter().flatten())
        .collect()
}

fn content(body: &Value) -> &str {
    body["content"]
        .as_str()
        .expect("private human-readable feedback")
}

fn mentions_blocked(body: &Value) {
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

fn bounded_private_reply(body: &Value) {
    let text = content(body);
    assert!(
        !text.trim().is_empty() && text.chars().count() <= 2000,
        "bounded Discord reply, got {} characters",
        text.chars().count()
    );
    mentions_blocked(body);
    let lower = text.to_lowercase();
    assert!(
        [
            "not changed",
            "no roles",
            "nothing was changed",
            "nothing changed",
            "no selection",
            "no changes",
            "did not change",
            "didn't change",
            "not applied"
        ]
        .iter()
        .any(|word| lower.contains(word)),
        "preview feedback must truthfully say no change was applied: {text}"
    );
    assert!(
        !lower.contains("may already"),
        "a no-effect preview cannot report uncertain applied changes: {text}"
    );
    assert!(
        !text.contains("<@") && !text.contains("@everyone") && !text.contains("@here"),
        "do not reflect mention-like untrusted input: {text}"
    );
    assert!(
        !text.contains("Game roles saved.") && !text.contains("Done. Here is where to go:"),
        "preview cannot borrow applied-choice success copy: {text}"
    );
}

fn preview_label(body: &Value) {
    bounded_private_reply(body);
    assert!(
        content(body).contains("Preview — roles not changed"),
        "preview must identify its no-effect semantics: {}",
        content(body)
    );
    for control in controls(body) {
        assert_ne!(
            control["custom_id"], GAME_SELECT_ID,
            "a preview result must not silently apply through the normal select"
        );
    }
}

fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    // Only the three outcome names are pinned, not formatting or prose.
    let lower = text.to_ascii_lowercase();
    let start = lower
        .find(heading)
        .unwrap_or_else(|| panic!("missing {heading} section: {text}"))
        + heading.len();
    let end = ["additions", "removals", "unchanged"]
        .iter()
        .filter(|h| **h != heading)
        .filter_map(|h| lower[start..].find(h).map(|offset| start + offset))
        .min()
        .unwrap_or(text.len());
    &text[start..end]
}

fn diff(body: &Value, additions: &[&str], removals: &[&str], unchanged: &[&str]) {
    preview_label(body);
    for (heading, labels) in [
        ("additions", additions),
        ("removals", removals),
        ("unchanged", unchanged),
    ] {
        let part = section(content(body), heading);
        for label in GAME_PICKS
            .iter()
            .chain(PLATFORM_PICKS.iter())
            .map(|pick| pick.label)
            .chain(["Unrelated", UNRELATED])
        {
            assert_eq!(
                part.contains(label),
                labels.contains(&label),
                "{heading} has wrong actual set delta: {part}"
            );
            if labels.contains(&label) {
                assert_eq!(
                    part.matches(label).count(),
                    1,
                    "each proposed choice appears once in {heading}"
                );
            }
        }
        if labels.is_empty() {
            assert!(
                ["none", "no ", "—", "empty"]
                    .iter()
                    .any(|word| part.to_lowercase().contains(word)),
                "empty {heading} must be explicit: {part}"
            );
        }
    }
}

// Statement-level triggers observe even zero-row DML and rolled-back attempts.
// Sequence advancement is not transactional; a swallowed store error cannot
// turn a mutating preview into a false no-write pass. Only disposable DBs.
async fn install_write_spy(pool: &PgPool) {
    sqlx::raw_sql("CREATE SEQUENCE preview_write_attempts;
        CREATE FUNCTION preview_observe_write() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN PERFORM nextval('preview_write_attempts'); RETURN NULL; END $$;
        DO $$ DECLARE target record; BEGIN
          FOR target IN SELECT tablename FROM pg_tables WHERE schemaname = 'public' LOOP
            EXECUTE format('CREATE TRIGGER preview_write_spy BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION preview_observe_write()', target.tablename);
          END LOOP;
        END $$;")
        .execute(pool).await.expect("install write-attempt observer on all fixture tables");
    let covered: Vec<String> = sqlx::query_scalar("SELECT c.relname FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid WHERE t.tgname='preview_write_spy'")
        .fetch_all(pool).await.unwrap();
    for table in [
        "events",
        "members",
        "tickets",
        "operational_audit_log",
        "guild_settings",
        "guild_settings_audit",
    ] {
        assert!(
            covered.iter().any(|name| name == table),
            "write spy missing {table}"
        );
    }
}

async fn no_db_writes(pool: &PgPool) {
    let attempted: bool = sqlx::query_scalar("SELECT is_called FROM preview_write_attempts")
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(
        !attempted,
        "preview attempted database mutation (including rollback/zero-row DML)"
    );
}

async fn no_effects(mock: &MockRest, pool: &PgPool, start: usize) {
    no_db_writes(pool).await;
    for request in &mock.requests()[start..] {
        let callback = request.method == "POST" && request.path.ends_with("/callback");
        let edit = request.method == "PATCH"
            && request.path.starts_with("/api/v10/webhooks/111/")
            && request.path.ends_with("/messages/@original");
        assert!(
            request.method == "GET" || callback || edit,
            "preview attempted non-private effect: {} {}",
            request.method,
            request.path
        );
        if callback {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(
                body["type"], 5,
                "one deferred private response, never a public send"
            );
            assert_eq!(body["data"]["flags"], 64);
            if body["data"].get("allowed_mentions").is_some() {
                mentions_blocked(&body["data"]);
            }
        }
        if edit {
            mentions_blocked(&serde_json::from_slice::<Value>(&request.body).unwrap());
        }
    }
}

async fn submit(
    runtime: &OnboardingRuntime,
    mock: &MockRest,
    pool: &PgPool,
    interaction: Interaction,
) -> Value {
    assert!(
        runtime.accepts_interaction(&interaction),
        "onboarding must own the preview interaction at ingress"
    );
    let start = mock.requests().len();
    let (result, queries) = Queries::capture(
        runtime
            .handle(OnboardingJob::Interaction(Box::new(interaction)), NOW)
            .boxed(),
    )
    .await;
    result.expect("private onboarding operation finishes");
    no_effects(mock, pool, start).await;
    for query in queries {
        let sql = query.normalized();
        assert!(
            !["insert ", "update ", "delete ", "truncate "]
                .iter()
                .any(|verb| sql.starts_with(verb)),
            "preview executed a mutating store statement"
        );
    }
    let requests = mock.requests();
    assert_eq!(
        requests[start..]
            .iter()
            .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
            .count(),
        1,
        "one ingress ACK, no double callback"
    );
    assert!(
        requests[start].path.ends_with("/callback"),
        "defer before role/config HTTP reads"
    );
    assert_eq!(
        requests[start..]
            .iter()
            .filter(|r| r.method == "PATCH")
            .count(),
        1,
        "finish the private defer exactly once"
    );
    last_edit(mock)
}

async fn open_preview(runtime: &OnboardingRuntime, mock: &MockRest, pool: &PgPool) -> PreviewMenu {
    let mut body = submit(runtime, mock, pool, reopen_games(300)).await;
    let entries: Vec<_> = controls(&body)
        .into_iter()
        .filter(|c| {
            ["label", "placeholder", "custom_id"].iter().any(|key| {
                c[key]
                    .as_str()
                    .is_some_and(|s| s.to_lowercase().contains("preview"))
            })
        })
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "existing game picker needs one clear self-only Preview entry; got {}",
        body
    );
    let entry = entries[0];
    let id = entry["custom_id"]
        .as_str()
        .expect("preview control ID")
        .to_owned();
    let kind = entry["type"].as_u64().unwrap();
    assert_ne!(id, GAME_SELECT_ID, "preview must not use the apply route");
    if kind == 2 {
        assert!(
            entry["label"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("preview"),
            "preview button needs a human-readable label"
        );
        body = submit(
            runtime,
            mock,
            pool,
            component_from_body(&body, &id, 2, &[], 301),
        )
        .await;
    } else {
        assert_eq!(kind, 3, "a preview entry is a button or a text select");
        assert!(
            entry["placeholder"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("preview"),
            "preview select needs a human-readable placeholder"
        );
    }
    let menus: Vec<_> = controls(&body)
        .into_iter()
        .filter(|c| c["type"] == 3 && c["custom_id"] != GAME_SELECT_ID)
        .collect();
    assert_eq!(
        menus.len(),
        1,
        "preview choices must have a distinct no-effect submission"
    );
    let menu = menus[0];
    assert_eq!(
        menu["min_values"], 0,
        "empty proposed answer is distinct from unknown-only input"
    );
    let options = menu["options"].as_array().expect("current game catalog");
    assert!(!options.is_empty() && options.len() <= 25);
    let mut keys = HashSet::new();
    for option in options {
        let key = option["value"].as_str().unwrap();
        assert!(
            GAME_PICKS.iter().any(|pick| pick.key == key),
            "only configured game keys, not arbitrary/platform roles: {key}"
        );
        assert!(keys.insert(key), "no duplicate catalog option");
    }
    assert_eq!(
        keys,
        GAME_PICKS.iter().map(|pick| pick.key).collect(),
        "all currently available configured games must be usable, not only forged by tests"
    );
    let maximum = menu["max_values"].as_u64().unwrap_or(1);
    assert!(
        maximum >= options.len() as u64 && maximum <= 25,
        "the UI must permit the full configured multi-choice answer"
    );
    let id = menu["custom_id"].as_str().unwrap().to_owned();
    PreviewMenu { id, body }
}

#[derive(Clone, Copy)]
enum Case {
    Delta,
    Fresh,
    Unchanged,
    Empty,
    Unknown,
    Duplicates,
    Unavailable,
    CatalogChanged,
    Disallowed,
    Authorization,
    ApplyFresh,
    LegacyControl,
    ApplyControl,
    Bounds,
    CallbackFailure,
    SpyControl,
}

async fn run_case(case: Case) {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert_ne!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"), "CI must configure the guarded disposable database; missing configuration is not a passing acceptance run");
        eprintln!("SKIP game-role preview DB acceptance: set guarded TWO_TEST_DATABASE_URL; no behavior was verified");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    // Keep the combined case future off the small default test-thread stack.
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

async fn exercise(case: Case, pool: &PgPool) {
    let state = Arc::new(Mutex::new(PreviewState {
        held: [
            role_id_of("shooters"),
            role_id_of("survival"),
            UNRELATED.into(),
        ]
        .into(),
        ..PreviewState::default()
    }));
    let mock = preview_discord(state.clone()).await;
    let runtime = runtime(pool, &mock, &vars("legacy", false));
    install_write_spy(pool).await;
    if matches!(case, Case::SpyControl) {
        let mut transaction = pool.begin().await.unwrap();
        sqlx::query("DELETE FROM events WHERE false")
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.rollback().await.unwrap();
        let attempted: bool = sqlx::query_scalar("SELECT is_called FROM preview_write_attempts")
            .fetch_one(pool)
            .await
            .unwrap();
        assert!(attempted, "spy must observe even rolled-back zero-row DML");
        return;
    }
    if matches!(case, Case::LegacyControl) {
        let before = state.lock().unwrap().held.clone();
        let body = submit(&runtime, &mock, pool, reopen_games(310)).await;
        let normal = controls(&body)
            .into_iter()
            .find(|c| c["custom_id"] == GAME_SELECT_ID)
            .expect("ordinary /reopen games survives");
        let defaults: HashSet<_> = normal["options"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["default"] == true)
            .map(|o| o["value"].as_str().unwrap())
            .collect();
        assert_eq!(defaults, HashSet::from(["shooters", "survival"]));
        let out = adjudicate_game_select(
            OnboardingMode::Legacy,
            &["retired"],
            &["1051272877871222915", "770"],
            &|_| true,
            22,
            false,
        )
        .unwrap();
        assert!(out.add_role_ids.is_empty() && out.remove_role_ids.is_empty());
        assert!(!out.record_selected && !out.record_routed);
        assert_eq!(state.lock().unwrap().held, before);
        return;
    }
    if matches!(case, Case::ApplyControl) {
        runtime
            .handle(
                OnboardingJob::Interaction(Box::new(select(
                    GAME_SELECT_ID,
                    3,
                    &["horror", "retired-key"],
                    311,
                ))),
                NOW,
            )
            .boxed()
            .await
            .unwrap();
        let writes: HashSet<_> = mock
            .requests()
            .into_iter()
            .filter(|r| matches!(r.method.as_str(), "PUT" | "DELETE"))
            .map(|r| (r.method, r.path.rsplit('/').next().unwrap().to_owned()))
            .collect();
        assert_eq!(
            writes,
            HashSet::from([
                ("PUT".into(), role_id_of("horror")),
                ("DELETE".into(), role_id_of("shooters")),
                ("DELETE".into(), role_id_of("survival"))
            ])
        );
        assert_eq!(
            state.lock().unwrap().held,
            HashSet::from([role_id_of("horror"), UNRELATED.into()])
        );
        let selected: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE event_type='game_roles_selected'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            selected, 1,
            "existing mixed known/stale apply policy is unchanged"
        );
        return;
    }
    let id = open_preview(&runtime, &mock, pool).await;
    let before = state.lock().unwrap().held.clone();
    match case {
        Case::Delta => {
            let body = submit(
                &runtime,
                &mock,
                pool,
                preview_select(&id, 3, &["survival", "horror"], 400),
            )
            .await;
            diff(&body, &["Horror"], &["Shooters"], &["Survival"]);
        }
        Case::Fresh => {
            state.lock().unwrap().held = [role_id_of("horror"), UNRELATED.into()].into();
            let body = submit(
                &runtime,
                &mock,
                pool,
                preview_select(&id, 3, &["horror", "survival"], 401),
            )
            .await;
            diff(&body, &["Survival"], &[], &["Horror"]);
            assert_eq!(
                state.lock().unwrap().held,
                HashSet::from([role_id_of("horror"), UNRELATED.into()])
            );
            return;
        }
        Case::Unchanged => {
            let body = submit(
                &runtime,
                &mock,
                pool,
                preview_select(&id, 3, &["shooters", "survival"], 402),
            )
            .await;
            diff(&body, &[], &[], &["Shooters", "Survival"]);
        }
        Case::Empty => {
            let body = submit(&runtime, &mock, pool, preview_select(&id, 3, &[], 403)).await;
            diff(&body, &[], &["Shooters", "Survival"], &[]);
        }
        Case::Unknown => {
            let oversized = format!("@everyone <@&770> {}", "x".repeat(8000));
            for (index, keys) in [
                vec!["retired-key"],
                vec!["770"],
                vec!["pc"],
                vec!["horror", "retired-key"],
                vec![oversized.as_str()],
            ]
            .into_iter()
            .enumerate()
            {
                let body = submit(
                    &runtime,
                    &mock,
                    pool,
                    preview_select(&id, 3, &keys, 404 + index as u64),
                )
                .await;
                bounded_private_reply(&body);
                let lower = content(&body).to_lowercase();
                assert!(
                    [
                        "unknown",
                        "stale",
                        "available",
                        "valid",
                        "recogniz",
                        "invalid",
                        "configured",
                        "game"
                    ]
                    .iter()
                    .any(|word| lower.contains(word)),
                    "explain rejected choices: {lower}"
                );
                assert!(
                    ["again", "reopen", "choose", "open", "retry"]
                        .iter()
                        .any(|word| lower.contains(word)),
                    "actionable private recovery: {lower}"
                );
                assert!(
                    !lower.contains("cleared") && !lower.contains("saved"),
                    "invalid preview is not a clear or apply: {lower}"
                );
                if !keys.contains(&"horror") && lower.contains("removals") {
                    for label in ["Shooters", "Survival"] {
                        assert!(
                            !section(content(&body), "removals").contains(label),
                            "unknown-only preview must not propose clearing held games"
                        );
                    }
                }
                assert_eq!(state.lock().unwrap().held, before);
            }
        }
        Case::Duplicates => {
            let body = submit(
                &runtime,
                &mock,
                pool,
                preview_select(&id, 3, &["survival", "horror", "horror"], 405),
            )
            .await;
            // Normal pure policy deduplicates. Preview may instead reject a
            // malformed duplicate explicitly, but cannot count/grant it twice.
            bounded_private_reply(&body);
            if content(&body).to_lowercase().contains("additions") {
                diff(&body, &["Horror"], &["Shooters"], &["Survival"]);
                assert_eq!(
                    section(content(&body), "additions")
                        .matches("Horror")
                        .count(),
                    1
                );
            } else {
                assert!(content(&body).to_lowercase().contains("duplicate"));
                assert!(content(&body).to_lowercase().contains("again"));
            }
        }
        Case::Unavailable => {
            for failure in 0..6 {
                {
                    let mut state = state.lock().unwrap();
                    state.member_status = match failure {
                        0 => Some(404),
                        1 => Some(403),
                        2 => Some(503),
                        _ => None,
                    };
                    state.roles_status = (failure == 3).then_some(403);
                    state.wrong_member = failure == 4;
                    state.malformed_member = failure == 5;
                }
                let body = submit(
                    &runtime,
                    &mock,
                    pool,
                    preview_select(&id, 3, &["horror"], 410 + failure),
                )
                .await;
                bounded_private_reply(&body);
                let lower = content(&body).to_lowercase();
                assert!(
                    [
                        "unavailable",
                        "couldn't",
                        "cannot",
                        "can't",
                        "unable",
                        "no longer"
                    ]
                    .iter()
                    .any(|word| lower.contains(word)),
                    "unreadable evidence is not empty membership: {lower}"
                );
                assert!(["again", "reopen", "retry", "open"]
                    .iter()
                    .any(|word| lower.contains(word)));
                assert!(!lower.contains("saved") && !lower.contains("cleared"));
            }
        }
        Case::CatalogChanged => {
            state.lock().unwrap().missing_role = Some(role_id_of("horror"));
            let body = submit(
                &runtime,
                &mock,
                pool,
                preview_select(&id, 3, &["horror"], 420),
            )
            .await;
            bounded_private_reply(&body);
            let lower = content(&body).to_lowercase();
            assert!(
                [
                    "no longer",
                    "unavailable",
                    "not available",
                    "stale",
                    "missing"
                ]
                .iter()
                .any(|word| lower.contains(word)),
                "deleted role must not be advertised as an available addition: {lower}"
            );
            assert!(["again", "reopen", "open", "choose"]
                .iter()
                .any(|word| lower.contains(word)));
        }
        Case::Disallowed => {
            for failure in 0..3 {
                {
                    let mut state = state.lock().unwrap();
                    state.managed_role = (failure == 0).then(|| role_id_of("horror"));
                    state.high_role = (failure == 1).then(|| role_id_of("horror"));
                    state.bot_cannot_manage = failure == 2;
                }
                let body = submit(
                    &runtime,
                    &mock,
                    pool,
                    preview_select(&id, 3, &["horror"], 430 + failure),
                )
                .await;
                bounded_private_reply(&body);
                let lower = content(&body).to_lowercase();
                assert!(
                    [
                        "permission",
                        "manage",
                        "hierarchy",
                        "unavailable",
                        "can't",
                        "cannot",
                        "allowed",
                        "configured"
                    ]
                    .iter()
                    .any(|word| lower.contains(word)),
                    "preview must not promise an unmanageable role: {lower}"
                );
                assert!(!lower.contains("saved") && !lower.contains("cleared"));
            }
        }
        Case::Authorization => {
            for failure in 0..3 {
                let mut wire =
                    serde_json::to_value(preview_select(&id, 3, &["horror"], 440 + failure))
                        .unwrap();
                match failure {
                    0 => {
                        wire["guild_id"] = json!("23");
                        wire["authorizing_integration_owners"] = json!({"0":"23"});
                    }
                    1 => wire["member"]["user"]["bot"] = json!(true),
                    _ => wire["member"] = Value::Null,
                }
                let interaction: Interaction = serde_json::from_value(wire).unwrap();
                let start = mock.requests().len();
                assert!(
                    !runtime.accepts_interaction(&interaction),
                    "foreign guild/bot/memberless input must not be captured"
                );
                runtime
                    .handle(OnboardingJob::Interaction(Box::new(interaction)), NOW)
                    .boxed()
                    .await
                    .unwrap();
                assert_eq!(
                    mock.requests().len(),
                    start,
                    "ineligible actor causes no HTTP reads or sends"
                );
                no_db_writes(pool).await;
            }
        }
        Case::ApplyFresh => {
            let body = submit(
                &runtime,
                &mock,
                pool,
                preview_select(&id, 3, &["horror", "survival"], 450),
            )
            .await;
            diff(&body, &["Horror"], &["Shooters"], &["Survival"]);
            state.lock().unwrap().held = [
                role_id_of("shooters"),
                role_id_of("horror"),
                UNRELATED.into(),
            ]
            .into();
            // A separate ordinary operation must re-read current roles. Never
            // turn the old preview delta into permission or a write plan.
            let start = mock.requests().len();
            runtime
                .handle(
                    OnboardingJob::Interaction(Box::new(select(
                        GAME_SELECT_ID,
                        3,
                        &["survival"],
                        451,
                    ))),
                    NOW + 1,
                )
                .boxed()
                .await
                .unwrap();
            let requests = mock.requests();
            let writes: HashSet<_> = requests[start..]
                .iter()
                .filter(|r| matches!(r.method.as_str(), "PUT" | "DELETE"))
                .map(|r| (r.method.as_str(), r.path.rsplit('/').next().unwrap()))
                .collect();
            assert_eq!(
                writes,
                HashSet::from([
                    ("PUT", "1179233034713702511"),
                    ("DELETE", "1051272877871222915"),
                    ("DELETE", "1119666971584237679")
                ])
            );
            assert_eq!(
                state.lock().unwrap().held,
                HashSet::from([role_id_of("survival"), UNRELATED.into()])
            );
            let selected: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM events WHERE event_type='game_roles_selected'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            assert_eq!(selected, 1, "only the separate real apply records a choice");
            return;
        }
        Case::CallbackFailure => {
            state.lock().unwrap().reject_callback = true;
            let start = mock.requests().len();
            runtime
                .handle(
                    OnboardingJob::Interaction(Box::new(preview_select(&id, 3, &["horror"], 460))),
                    NOW,
                )
                .boxed()
                .await
                .unwrap();
            no_effects(&mock, pool, start).await;
            assert_eq!(
                mock.requests()[start..]
                    .iter()
                    .filter(|r| r.path.ends_with("/callback"))
                    .count(),
                1,
                "unconfirmed ACK is not retried"
            );
            assert!(
                !mock.requests()[start..].iter().any(|r| r.method == "GET"),
                "unconfirmed preview does not read/evaluate a role plan"
            );
            let requests = mock.requests();
            let edits: Vec<_> = requests[start..]
                .iter()
                .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
                .collect();
            assert_eq!(
                edits.len(),
                1,
                "unconfirmed operation must complete its own private reply, not reuse an old edit"
            );
            let body: Value = serde_json::from_slice(&edits[0].body).unwrap();
            bounded_private_reply(&body);
            let lower = content(&body).to_lowercase();
            assert!(
                ["confirm", "verify"]
                    .iter()
                    .any(|word| lower.contains(word))
                    && lower.contains("again"),
                "truthful unconfirmed-preview recovery guidance"
            );
        }
        Case::Bounds => {
            let keys: Vec<_> = GAME_PICKS.iter().map(|pick| pick.key).collect();
            let body = submit(&runtime, &mock, pool, preview_select(&id, 3, &keys, 470)).await;
            let additions: Vec<_> = GAME_PICKS
                .iter()
                .filter(|pick| !matches!(pick.key, "shooters" | "survival"))
                .map(|pick| pick.label)
                .collect();
            diff(&body, &additions, &[], &["Shooters", "Survival"]);
            let oversized: Vec<_> = std::iter::repeat_n("horror", 100).collect();
            let body = submit(
                &runtime,
                &mock,
                pool,
                preview_select(&id, 3, &oversized, 471),
            )
            .await;
            bounded_private_reply(&body);
            assert!(
                ["many", "limit", "duplicate", "invalid", "again"]
                    .iter()
                    .any(|word| content(&body).to_lowercase().contains(word)),
                "oversized selection needs actionable private feedback"
            );
        }
        Case::LegacyControl | Case::ApplyControl | Case::SpyControl => unreachable!(),
    }
    assert_eq!(
        state.lock().unwrap().held,
        before,
        "preview never changes member roles"
    );
    no_db_writes(pool).await;
}

#[tokio::test]
async fn preview_projects_actual_additions_removals_and_unchanged_not_apply_effects() {
    run_case(Case::Delta).await;
}
#[tokio::test]
async fn preview_reads_roles_fresh_after_the_menu_was_opened() {
    run_case(Case::Fresh).await;
}
#[tokio::test]
async fn preview_of_unchanged_choices_is_truthfully_no_change() {
    run_case(Case::Unchanged).await;
}
#[tokio::test]
async fn preview_of_raw_empty_choices_lists_removals_without_clearing() {
    run_case(Case::Empty).await;
}
#[tokio::test]
async fn preview_rejects_unknown_raw_role_platform_mixed_and_oversized_choices_privately() {
    run_case(Case::Unknown).await;
}
#[tokio::test]
async fn preview_duplicates_cannot_duplicate_a_proposed_grant() {
    run_case(Case::Duplicates).await;
}
#[tokio::test]
async fn preview_read_failures_and_foreign_member_evidence_never_mean_empty_roles() {
    run_case(Case::Unavailable).await;
}
#[tokio::test]
async fn preview_rechecks_catalog_after_a_role_was_deleted() {
    run_case(Case::CatalogChanged).await;
}
#[tokio::test]
async fn preview_does_not_promise_managed_high_or_unmanageable_roles() {
    run_case(Case::Disallowed).await;
}
#[tokio::test]
async fn preview_dispatch_refuses_foreign_guild_bot_and_memberless_actors() {
    run_case(Case::Authorization).await;
}
#[tokio::test]
async fn preview_is_not_authorization_for_a_later_ordinary_apply() {
    run_case(Case::ApplyFresh).await;
}
#[tokio::test]
async fn preview_unconfirmed_ack_never_retries_or_evaluates_choices() {
    run_case(Case::CallbackFailure).await;
}
#[tokio::test]
async fn preview_bounds_full_catalog_and_rejects_oversized_submissions() {
    run_case(Case::Bounds).await;
}
#[tokio::test]
async fn control_ordinary_apply_still_writes_roles_and_records_valid_subset() {
    run_case(Case::ApplyControl).await;
}
#[tokio::test]
async fn control_ordinary_reopen_preselection_and_unknown_only_policy_are_unchanged() {
    run_case(Case::LegacyControl).await;
}
#[tokio::test]
async fn control_write_spy_detects_rolled_back_zero_row_dml() {
    run_case(Case::SpyControl).await;
}
