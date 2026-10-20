//! Protected acceptance: unverified live bot flags cannot become human attendance.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{rsvp::handle_rsvp_interaction, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const EVENT: &str = "555555555555555555";
const HOST: &str = "6666";
const TARGET: &str = "7777";
const REFUSAL: &str = "Unable to verify attendance member.";

// Discord omits `bot` for humans (`bot?` is optional, never null), so an absent
// key keeps the documented human default; only a present non-boolean value is
// unverifiable.
fn invalid_flags() -> Vec<(&'static str, Value)> {
    vec![
        ("null", Value::Null),
        ("string_false", json!("false")),
        ("string_true", json!("true")),
        ("zero", json!(0)),
        ("one", json!(1)),
        ("array", json!([])),
        ("object", json!({})),
    ]
}

fn interaction(on_behalf: bool, occurrence: &str) -> Interaction {
    let target = if on_behalf { TARGET } else { HOST };
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": "3333", "token": "attendance-fixture", "type": 2, "version": 1,
        "guild_id": GUILD,
        "member": {"permissions": (1_u64 << 33).to_string(), "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": HOST, "username": "host", "discriminator": "0"}},
        "data": {"id": "4444", "name": "attendance", "type": 1,
            "options": [
                {"name": "event-occurrence", "type": 3, "value": occurrence},
                {"name": "member", "type": 6, "value": target}],
            "resolved": {"users": {target: {
                "id": target, "username": "target", "discriminator": "0", "bot": false}}}}
    }))
    .expect("valid host interaction with a deliberately stale resolved human")
}

async fn checkin(pool: &PgPool, flag: Option<Value>, on_behalf: bool, label: &str) -> String {
    let target = if on_behalf { TARGET } else { HOST };
    let mut live_user = json!({"id": target, "username": "target", "discriminator": "0"});
    if let Some(flag) = flag {
        live_user["bot"] = flag;
    }
    let mock = MockRest::with_responder(move |request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", _) => ScriptedResponse::status(204),
            ("PATCH", _) => ScriptedResponse::json(200, json!({"id": "99"})),
            ("GET", path) if path.ends_with(&format!("/scheduled-events/{EVENT}")) => {
                ScriptedResponse::json(200, json!({"id": EVENT, "guild_id": GUILD, "status": 1}))
            }
            ("GET", path) if path.ends_with(&format!("/members/{target}")) => {
                ScriptedResponse::json(200, json!({"user": live_user, "roles": []}))
            }
            ("GET", path) if path.ends_with(&format!("/members/{HOST}")) => ScriptedResponse::json(
                200,
                json!({"user": {"id": HOST, "bot": false}, "roles": []}),
            ),
            _ => ScriptedResponse::status(500),
        }
    })
    .await;
    let router = InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().expect("synthetic guild")),
        scorecard: true,
        automations: false,
        announcements: true,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    let executor = ActionExecutor::with_proxy("fixture".to_owned(), Some(mock.origin()))
        .expect("loopback executor");
    assert!(handle_rsvp_interaction(
        &router,
        pool,
        &executor,
        &ClassifierConfig::default(),
        &interaction(on_behalf, &format!("{EVENT}:{label}")),
    )
    .await
    .expect("attendance interaction completes"));
    let reply: Value = serde_json::from_slice(
        &mock
            .requests()
            .iter()
            .find(|request| request.method == "PATCH")
            .expect("deferred reply edited")
            .body,
    )
    .expect("reply JSON");
    let content = reply["content"].as_str().expect("reply content").to_owned();
    mock.shutdown().await;
    content
}

async fn fixture() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => return None,
        Err(_) => panic!("test database URL must be Unicode"),
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("guarded disposable database"),
    )
}

#[tokio::test]
async fn unknown_live_flags_refuse_before_the_store_even_when_the_store_is_unavailable() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_closed")
        .expect("lazy pool does not connect");
    pool.close().await;
    let mut observed = Vec::new();
    for (name, flag) in invalid_flags() {
        for on_behalf in [false, true] {
            let reply = checkin(&pool, Some(flag.clone()), on_behalf, name).await;
            observed.push((name, on_behalf, reply));
        }
    }
    assert!(
        observed.iter().all(|(_, _, reply)| reply == REFUSAL),
        "unknown bot flags must refuse before record_checkin, not fall through to a store error: {observed:?}"
    );
}

#[tokio::test]
async fn unknown_live_flags_refuse_with_zero_checkin_insert_attempts() {
    let Some(db) = fixture().await else {
        eprintln!("disposable DB acceptance not run: TWO_TEST_DATABASE_URL unset");
        return;
    };
    // Sequence increments survive transaction rollback and compensation. A zero
    // final row count alone would miss an insert followed by compensating delete.
    sqlx::raw_sql(
        "CREATE SEQUENCE acceptance_checkin_inserts;
         CREATE FUNCTION acceptance_count_checkin() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM nextval('acceptance_checkin_inserts'); RETURN NEW; END $$;
         CREATE TRIGGER acceptance_checkin_observer BEFORE INSERT ON community_facts
         FOR EACH ROW EXECUTE FUNCTION acceptance_count_checkin();",
    )
    .execute(db.pool())
    .await
    .expect("install fixture-only nontransactional write witness");
    let mut observed = Vec::new();
    for (name, flag) in invalid_flags() {
        for on_behalf in [false, true] {
            let reply = checkin(
                db.pool(),
                Some(flag.clone()),
                on_behalf,
                &format!("{name}-{on_behalf}"),
            )
            .await;
            let attempts: i64 = sqlx::query_scalar(
                "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM acceptance_checkin_inserts",
            )
            .fetch_one(db.pool())
            .await
            .expect("read write witness");
            let facts: i64 = sqlx::query_scalar("SELECT count(*) FROM community_facts")
                .fetch_one(db.pool())
                .await
                .expect("read persisted facts");
            observed.push((name, on_behalf, reply, attempts, facts));
        }
    }
    db.close().await.expect("drop owned disposable database");
    assert!(
        observed.iter().all(|(_, _, reply, attempts, facts)| reply == REFUSAL && *attempts == 0 && *facts == 0),
        "refusal must precede every record_checkin insert, including compensated writes: {observed:?}"
    );
}

#[tokio::test]
async fn explicit_boolean_live_flags_preserve_host_proof_and_human_totals() {
    let Some(db) = fixture().await else {
        eprintln!("disposable DB acceptance not run: TWO_TEST_DATABASE_URL unset");
        return;
    };
    let human_reply = checkin(db.pool(), Some(json!(false)), false, "human").await;
    let bot_reply = checkin(db.pool(), Some(json!(true)), true, "bot").await;
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT actor_id, classification, classifier_version, matched_rule, metadata
         FROM community_facts ORDER BY actor_id",
    )
    .fetch_all(db.pool())
    .await
    .expect("read actual checkins");
    let humans: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM community_facts WHERE classification = 'eligible_human'",
    )
    .fetch_one(db.pool())
    .await
    .expect("human attendance total");
    db.close().await.expect("drop owned disposable database");
    assert_eq!(
        human_reply,
        "Recorded <@6666> for event occurrence `555555555555555555:human`."
    );
    assert_eq!(
        bot_reply,
        "Recorded <@7777> for event occurrence `555555555555555555:bot`."
    );
    assert_eq!(humans, 1);
    assert_eq!(rows.len(), 2);
    for (row, actor, classification, occurrence) in [
        (&rows[0], HOST, "eligible_human", "555555555555555555:human"),
        (&rows[1], TARGET, "bot", "555555555555555555:bot"),
    ] {
        assert_eq!(row.0, actor);
        assert_eq!(row.1, classification);
        assert_eq!(row.2, "community-v1");
        assert_eq!(
            serde_json::from_str::<Value>(&row.4).expect("proof JSON"),
            json!({"eventOccurrenceId": occurrence, "proof": "host_checkin"})
        );
    }
}

#[tokio::test]
async fn absent_live_flag_keeps_discords_documented_human_default() {
    let Some(db) = fixture().await else {
        eprintln!("disposable DB acceptance not run: TWO_TEST_DATABASE_URL unset");
        return;
    };
    let self_reply = checkin(db.pool(), None, false, "absent-self").await;
    let member_reply = checkin(db.pool(), None, true, "absent-member").await;
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT actor_id, classification, metadata FROM community_facts ORDER BY actor_id",
    )
    .fetch_all(db.pool())
    .await
    .expect("read actual checkins");
    db.close().await.expect("drop owned disposable database");
    assert_eq!(
        self_reply,
        "Recorded <@6666> for event occurrence `555555555555555555:absent-self`."
    );
    assert_eq!(
        member_reply,
        "Recorded <@7777> for event occurrence `555555555555555555:absent-member`."
    );
    assert_eq!(rows.len(), 2);
    for (row, actor, occurrence) in [
        (&rows[0], HOST, "555555555555555555:absent-self"),
        (&rows[1], TARGET, "555555555555555555:absent-member"),
    ] {
        assert_eq!(row.0, actor);
        assert_eq!(row.1, "eligible_human");
        assert_eq!(
            serde_json::from_str::<Value>(&row.2).expect("proof JSON"),
            json!({"eventOccurrenceId": occurrence, "proof": "host_checkin"})
        );
    }
}
