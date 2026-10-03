//! Shared route → sqlx store → shared executor acceptance, never live Discord.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    Pool, Postgres,
};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{
    interactions::InteractionRuntime, rsvp::handle_rsvp_interaction, ActionExecutor,
};

const GUILD: &str = "1545644954272137297";
const EVENT: &str = "1546451670500642999";
const USER: &str = "1546451670500642888";
const OTHER: &str = "1546451670500642777";
const MANAGE_EVENTS: u64 = 1 << 33;

fn router() -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
        scorecard: true,
        automations: false,
        announcements: true,
        moderation: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    })
}

fn slash(id: u64, name: &str, permissions: u64, options: Value) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": id.to_string(), "token": "mock-rsvp-token", "type": 2,
        "version": 1, "guild_id": GUILD,
        "member": {"permissions": permissions.to_string(), "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": USER, "username": "invoker", "discriminator": "0"}},
        "data": {"id": "4444", "name": name, "type": 1, "options": options,
            "resolved": {"users": {
                USER: {"id": USER, "username": "human", "discriminator": "0"},
                OTHER: {"id": OTHER, "username": "bot", "discriminator": "0", "bot": true}
            }}}
    }))
    .expect("wire interaction")
}

fn rsvp(id: u64, status: &str) -> Interaction {
    slash(
        id,
        "rsvp",
        0,
        json!([
            {"name": "event-id", "type": 3, "value": EVENT},
            {"name": "status", "type": 3, "value": status}
        ]),
    )
}

fn attendance(id: u64, permissions: u64, member: &str, occurrence: &str) -> Interaction {
    slash(
        id,
        "attendance",
        permissions,
        json!([
            {"name": "event-occurrence", "type": 3, "value": occurrence},
            {"name": "member", "type": 6, "value": member}
        ]),
    )
}

fn event(status: u64) -> ScriptedResponse {
    ScriptedResponse::json(
        200,
        json!({"id": EVENT, "guild_id": GUILD, "status": status}),
    )
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy(
        "mock-token-not-a-credential".to_owned(),
        Some(mock.origin()),
    )
    .unwrap()
}

async fn pool() -> Option<(Pool<Postgres>, String)> {
    let url = match std::env::var("TWO_RSVP_RUNTIME_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("skipped database acceptance: TWO_RSVP_RUNTIME_TEST_DATABASE_URL not set");
            return None;
        }
        Err(_) => panic!("test URL must be Unicode"),
    };
    let options: PgConnectOptions = url.parse().expect("test options");
    assert_eq!(options.get_host(), "agent-testdb");
    assert_eq!(options.get_port(), 5432);
    assert_eq!(options.get_username(), "agent_test");
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let schema = format!(
        "rsvp_wire_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    assert!(
        schema.len() < 63
            && schema
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    );
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .expect("testdb");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../cutover/migrations/0160_rsvp.sql"))
        .execute(&pool)
        .await
        .unwrap();
    Some((pool, schema))
}

async fn cleanup(pool: Pool<Postgres>, schema: String) {
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

async fn counts(pool: &Pool<Postgres>) -> (i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM event_rsvps), (SELECT count(*) FROM announcements_audit_log), (SELECT count(*) FROM community_facts)")
        .fetch_one(pool).await.unwrap()
}

fn assert_reply(mock: &MockRest, content: &str, deferred: bool) {
    let requests = mock.requests();
    let last = requests.last().unwrap();
    let body: Value = serde_json::from_slice(&last.body).unwrap();
    if deferred {
        assert_eq!(last.method, "PATCH");
        assert_eq!(
            last.path,
            "/api/v10/webhooks/1111/mock-rsvp-token/messages/@original"
        );
        assert_eq!(body["content"], content);
        // Executor defaults continue to suppress mentions on deferred edits.
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    } else {
        assert_eq!(last.method, "POST");
        assert_eq!(body["type"], 4);
        assert_eq!(body["data"]["flags"], 64);
        assert_eq!(body["data"]["content"], content);
    }
}

fn runtime(pool: &Pool<Postgres>, mock: &MockRest) -> InteractionRuntime {
    InteractionRuntime::with_router(
        router(),
        pool.clone(),
        executor(mock),
        0,
        ClassifierConfig::default(),
    )
}

async fn run(pool: &Pool<Postgres>, mock: &MockRest, interaction: &Interaction) {
    assert!(runtime(pool, mock)
        .handle(interaction)
        .await
        .expect("handler execution"));
}

#[tokio::test]
async fn transitions_totals_and_audits_round_trip_through_router() {
    let Some((pool, schema)) = pool().await else {
        return;
    };
    let mock = MockRest::start(
        vec![
            // Deferred edits require ID-bearing 200 receipts (executor mutation_receipt_id).
            ScriptedResponse::status(204),
            event(1),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            event(2),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            event(3),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            event(1),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    for (i, status) in ["going", "interested", "declined", "declined"]
        .iter()
        .enumerate()
    {
        run(&pool, &mock, &rsvp(100 + i as u64, status)).await;
        assert_reply(&mock, &format!("RSVP saved: {status}."), true);
        let stored: String = sqlx::query_scalar(
            "SELECT status FROM event_rsvps WHERE guild_id=$1 AND event_id=$2 AND user_id=$3",
        )
        .bind(GUILD)
        .bind(EVENT)
        .bind(USER)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(&stored, status);
    }
    run(
        &pool,
        &mock,
        &slash(
            110,
            "rsvp-attendance",
            0,
            json!([{ "name": "event-id", "type": 3, "value": EVENT }]),
        ),
    )
    .await;
    assert_reply(&mock, "Going: 0\nInterested: 0\nDeclined: 1", true);
    assert_eq!(counts(&pool).await, (1, 4, 0));
    let audits: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT actor_id, action, target_key, outcome FROM announcements_audit_log ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for (row, status) in audits
        .iter()
        .zip(["going", "interested", "declined", "declined"])
    {
        assert_eq!(
            row,
            &(
                USER.into(),
                "event.rsvp".into(),
                EVENT.into(),
                status.into()
            )
        );
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 14);
    assert_eq!(requests[12].method, "POST");
    assert_eq!(requests[13].method, "PATCH");
    for chunk in requests[..12].chunks(3) {
        let ack: Value = serde_json::from_slice(&chunk[0].body).unwrap();
        assert_eq!(ack["type"], 5);
        assert_eq!(ack["data"]["flags"], 64);
        assert_eq!(chunk[1].method, "GET");
        assert_eq!(
            chunk[1].path,
            format!("/api/v10/guilds/{GUILD}/scheduled-events/{EVENT}")
        );
    }
    cleanup(pool, schema).await;
    mock.shutdown().await;
}

#[tokio::test]
async fn missing_cancelled_and_malformed_events_refuse_without_writes() {
    let Some((pool, schema)) = pool().await else {
        return;
    };
    for (lookup, reply) in [
        (
            ScriptedResponse::status(404),
            "No scheduled event with that id exists in this server.",
        ),
        (event(4), "That scheduled event is cancelled."),
        (
            ScriptedResponse::json(200, json!({"id": EVENT, "guild_id": "999", "status": 1})),
            "Discord returned an invalid scheduled event status.",
        ),
        (
            event(9),
            "Discord returned an invalid scheduled event status.",
        ),
        (
            ScriptedResponse {
                body: b"invalid-json".to_vec(),
                ..ScriptedResponse::status(200)
            },
            "Discord returned an invalid scheduled event status.",
        ),
        (
            ScriptedResponse::status(403),
            "Discord request failed: HTTP 403",
        ),
        (
            ScriptedResponse::status(429),
            "Discord request failed: HTTP 429",
        ),
        (
            ScriptedResponse::status(503),
            "Discord request failed: HTTP 503",
        ),
    ] {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::status(204),
                lookup,
                // Refusal text is still delivered as a deferred edit: ID receipt required.
                ScriptedResponse::json(200, json!({"id": "99"})),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        run(&pool, &mock, &rsvp(200, "going")).await;
        assert_reply(&mock, reply, true);
        assert_eq!(mock.requests().len(), 3);
        assert_eq!(counts(&pool).await, (0, 0, 0));
        mock.shutdown().await;
    }
    cleanup(pool, schema).await;
}

#[tokio::test]
async fn totals_remain_readable_without_live_event_access() {
    let Some((pool, schema)) = pool().await else {
        return;
    };
    two_bot_core::put_rsvp(
        &pool,
        &two_bot_core::RsvpRecord {
            guild_id: GUILD.into(),
            event_id: EVENT.into(),
            user_id: USER.into(),
            status: two_bot_core::RsvpStatus::Going,
            responded_at: two_bot_core::now_iso(),
        },
    )
    .await
    .unwrap();
    for unavailable in [
        ScriptedResponse::status(404),
        event(4),
        ScriptedResponse::status(403),
    ] {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::status(204),
                ScriptedResponse::json(200, json!({"id": "99"})),
            ],
            unavailable,
        )
        .await;
        run(
            &pool,
            &mock,
            &slash(
                202,
                "rsvp-attendance",
                0,
                json!([{ "name": "event-id", "type": 3, "value": EVENT }]),
            ),
        )
        .await;
        assert_reply(&mock, "Going: 1\nInterested: 0\nDeclined: 0", true);
        assert_eq!(mock.requests().len(), 2);
        assert!(mock.requests().iter().all(|r| r.method != "GET"));
        assert_eq!(counts(&pool).await, (1, 0, 0));
        mock.shutdown().await;
    }
    cleanup(pool, schema).await;
}

#[tokio::test]
async fn permissions_gates_and_guild_fence_precede_store_access() {
    let Some((pool, schema)) = pool().await else {
        return;
    };
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    run(&pool, &mock, &attendance(300, 0, USER, "weekly:2026-09-30")).await;
    assert_reply(&mock, "Manage Events permission is required.", false);
    assert_eq!(mock.requests().len(), 1);
    let off = InteractionRouter::new(RouterGates {
        scorecard: false,
        announcements: false,
        ..router().gates()
    });
    for interaction in [
        rsvp(301, "going"),
        attendance(302, MANAGE_EVENTS, USER, "weekly"),
    ] {
        handle_rsvp_interaction(
            &off,
            &pool,
            &executor(&mock),
            &ClassifierConfig::default(),
            &interaction,
        )
        .await
        .unwrap();
    }
    assert_reply(
        &mock,
        "Attendance capture is not enabled on this server.",
        false,
    );
    let mut foreign = rsvp(303, "going");
    foreign.guild_id = Some(twilight_model::id::Id::new(999));
    assert!(!handle_rsvp_interaction(
        &router(),
        &pool,
        &executor(&mock),
        &ClassifierConfig::default(),
        &foreign
    )
    .await
    .unwrap());
    foreign.guild_id = None;
    assert!(!handle_rsvp_interaction(
        &router(),
        &pool,
        &executor(&mock),
        &ClassifierConfig::default(),
        &foreign
    )
    .await
    .unwrap());
    assert_eq!(mock.requests().len(), 3);
    assert_eq!(counts(&pool).await, (0, 0, 0));
    cleanup(pool, schema).await;
    mock.shutdown().await;
}

#[tokio::test]
async fn host_checkin_identity_duplicate_and_classifier_are_preserved() {
    let Some((pool, schema)) = pool().await else {
        return;
    };
    // Four check-ins, each a 204 callback plus an ID-bearing 200 deferred edit.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(204),
    )
    .await;
    let occ = "weekly:2026-09-30";
    run(
        &pool,
        &mock,
        &attendance(400, MANAGE_EVENTS, USER, &format!(" {occ} ")),
    )
    .await;
    assert_reply(
        &mock,
        &format!("Recorded <@{USER}> for event occurrence `{occ}`."),
        true,
    );
    run(&pool, &mock, &attendance(401, MANAGE_EVENTS, USER, occ)).await;
    assert_reply(
        &mock,
        &format!("Attendance for <@{USER}> and event occurrence `{occ}` was already recorded."),
        true,
    );
    run(&pool, &mock, &attendance(402, MANAGE_EVENTS, OTHER, occ)).await;
    assert_eq!(counts(&pool).await, (0, 0, 2));
    let rows: Vec<(String, String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT event_type, source_event_id, actor_id, source, classification, metadata, idempotency_key FROM community_facts ORDER BY id")
        .fetch_all(&pool).await.unwrap();
    for (row, member, class) in [(&rows[0], USER, "eligible_human"), (&rows[1], OTHER, "bot")] {
        assert_eq!(row.0, "event_attended");
        assert_eq!(row.1, format!("{occ}:{member}"));
        assert_eq!(row.2, member);
        assert_eq!(row.3, format!("event:{occ}"));
        assert_eq!(row.4, class);
        assert_eq!(
            serde_json::from_str::<Value>(&row.5).unwrap(),
            json!({"eventOccurrenceId": occ, "proof": "host_checkin"})
        );
        assert_eq!(row.6, format!("event-attended:{occ}:{member}"));
    }
    let mut config = ClassifierConfig::default();
    config.test_actor_ids.insert(USER.into());
    handle_rsvp_interaction(
        &router(),
        &pool,
        &executor(&mock),
        &config,
        &attendance(403, MANAGE_EVENTS, USER, "next-occurrence"),
    )
    .await
    .unwrap();
    let verdict: (String, String, String) = sqlx::query_as("SELECT classifier_version, classification, matched_rule FROM community_facts WHERE source_event_id = $1")
        .bind(format!("next-occurrence:{USER}")).fetch_one(&pool).await.unwrap();
    assert_eq!(
        verdict,
        (
            "community-v1".into(),
            "test".into(),
            "configured_test_actor".into()
        )
    );
    assert!(mock.requests().iter().all(|r| r.method != "GET"));
    cleanup(pool, schema).await;
    mock.shutdown().await;
}

#[tokio::test]
async fn malformed_inputs_and_failed_ack_do_not_write() {
    let Some((pool, schema)) = pool().await else {
        return;
    };
    // Three malformed inputs, each a 204 callback plus an ID-bearing 200 error edit.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(204),
    )
    .await;
    run(&pool, &mock, &rsvp(500, "bogus")).await;
    run(&pool, &mock, &attendance(501, MANAGE_EVENTS, USER, "  ")).await;
    assert_reply(&mock, "event occurrence must not be empty.", true);
    run(
        &pool,
        &mock,
        &attendance(502, MANAGE_EVENTS, "1546451670500642111", "occ"),
    )
    .await;
    assert_reply(&mock, "Unable to resolve attendance member.", true);
    assert_eq!(counts(&pool).await, (0, 0, 0));
    assert_eq!(mock.requests().len(), 6);
    let denied = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    assert!(handle_rsvp_interaction(
        &router(),
        &pool,
        &executor(&denied),
        &ClassifierConfig::default(),
        &rsvp(503, "going")
    )
    .await
    .is_err());
    assert_eq!(denied.requests().len(), 1);
    assert_eq!(counts(&pool).await, (0, 0, 0));
    cleanup(pool, schema).await;
    denied.shutdown().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn failed_final_reply_does_not_retry_committed_effects() {
    let Some((pool, schema)) = pool().await else {
        return;
    };
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            ScriptedResponse::status(500),
            // Discord refuses a repeated acknowledgement for the same interaction.
            ScriptedResponse::status(400),
            ScriptedResponse::status(204),
            ScriptedResponse::status(500),
            ScriptedResponse::status(204),
            // Intended successful duplicate check-in edit: ID receipt required.
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&pool, &mock);
    let interaction = rsvp(600, "going");
    assert!(runtime.handle(&interaction).await.is_err());
    assert_eq!(counts(&pool).await, (1, 1, 0));
    assert_eq!(mock.requests().len(), 3);
    assert!(runtime.handle(&interaction).await.is_err());
    assert_eq!(counts(&pool).await, (1, 1, 0));
    assert_eq!(mock.requests().len(), 4);

    assert!(runtime
        .handle(&attendance(601, MANAGE_EVENTS, USER, "weekly"))
        .await
        .is_err());
    assert_eq!(counts(&pool).await, (1, 1, 1));
    assert_eq!(mock.requests().len(), 6);
    assert!(runtime
        .handle(&attendance(602, MANAGE_EVENTS, USER, "weekly"))
        .await
        .unwrap());
    assert_reply(
        &mock,
        &two_bot_core::checkin_duplicate_text(USER, "weekly"),
        true,
    );
    assert_eq!(counts(&pool).await, (1, 1, 1));
    cleanup(pool, schema).await;
    mock.shutdown().await;
}

#[tokio::test]
async fn shared_runtime_publishes_complete_registry_once() {
    let Some((pool, schema)) = pool().await else {
        return;
    };
    // Bare 200 triggers the mock's guild-command PUT echo self-heal, returning the
    // complete submitted command list as the registry receipt.
    let mock = MockRest::start(vec![], ScriptedResponse::status(200)).await;
    runtime(&pool, &mock).publish(1111).await.unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(
        requests[0].path,
        format!("/api/v10/applications/1111/guilds/{GUILD}/commands")
    );
    let commands: Vec<Value> = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(commands.len(), router().publish_set(&[]).unwrap().len());
    for name in ["attendance", "rsvp", "rsvp-attendance"] {
        assert_eq!(commands.iter().filter(|c| c["name"] == name).count(), 1);
    }
    let attendance = commands.iter().find(|c| c["name"] == "attendance").unwrap();
    assert_eq!(
        attendance["default_member_permissions"],
        MANAGE_EVENTS.to_string()
    );
    assert_eq!(counts(&pool).await, (0, 0, 0));
    cleanup(pool, schema).await;
    mock.shutdown().await;
}

#[test]
fn published_names_remain_unique() {
    let published = router().publish_set(&[]).unwrap();
    for name in ["attendance", "rsvp", "rsvp-attendance"] {
        assert_eq!(published.iter().filter(|c| c.name == name).count(), 1);
    }
}

#[tokio::test]
async fn mismatched_application_identity_is_refused_before_any_callback_or_store_work() {
    let Some((pool, schema)) = pool().await else {
        return;
    };
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&pool, &mock);
    runtime.set_application_id(2222);
    let mut foreign = rsvp(700, "going");
    foreign.application_id = twilight_model::id::Id::new(9999);
    // Same fence as `handle`/`handle_routed`: the split prepare path refuses
    // before any callback or store work, and complete stays a no-op.
    let prepared = runtime.prepare(foreign).await.expect("prepare refusal");
    assert!(!runtime.complete(prepared).await.expect("complete refusal"));
    assert!(mock.requests().is_empty());
    assert_eq!(counts(&pool).await, (0, 0, 0));
    cleanup(pool, schema).await;
    mock.shutdown().await;
}
