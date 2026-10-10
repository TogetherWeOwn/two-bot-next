//! Shared route → sqlx store → shared executor acceptance, never live Discord.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{Pool, Postgres};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{
    interactions::InteractionRuntime, rsvp::handle_rsvp_interaction, ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

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
        voice: false,
        voice_assistant: false,
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

/// RA-01 live-membership evidence: the member currently belongs to the guild.
fn member(user: &str) -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"user": {"id": user}, "roles": []}))
}

/// RA-02 anchored occurrence: the live event plus a repeat label. Bare slugs
/// name no anchorable event and refuse, so valid fixtures always anchor.
fn anchored(label: &str) -> String {
    format!("{EVENT}:{label}")
}

/// Live `GET /guilds/{g}/members/{u}` read: `bot: None` omits the key (Discord
/// omits it for humans), exercising the default-false path.
fn member_get(user: &str, bot: Option<bool>) -> ScriptedResponse {
    let mut member = json!({"id": user});
    if let Some(bot) = bot {
        member["bot"] = json!(bot);
    }
    ScriptedResponse::json(200, json!({"user": member, "roles": []}))
}

fn scheduled_event_path(event: &str) -> String {
    format!("/api/v10/guilds/{GUILD}/scheduled-events/{event}")
}

fn guild_member_path(user: &str) -> String {
    format!("/api/v10/guilds/{GUILD}/members/{user}")
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy(
        "mock-token-not-a-credential".to_owned(),
        Some(mock.origin()),
    )
    .unwrap()
}

/// Guarded disposable fixture: the shared testsupport guard validates the
/// bootstrap URL (explicit empty-password authority, disposable database name,
/// no query/fragment, no ambient libpq overrides) before any connection is
/// opened. Each test gets its own migrated database; `close` drops it.
struct Fixture {
    db: TestDatabase,
}

impl Fixture {
    fn pool(&self) -> &Pool<Postgres> {
        self.db.pool()
    }

    async fn close(self) {
        self.db.close().await.expect("drop test database");
    }
}

async fn pool() -> Option<Fixture> {
    let url = match std::env::var("TWO_RSVP_RUNTIME_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("skipped database acceptance: TWO_RSVP_RUNTIME_TEST_DATABASE_URL not set");
            return None;
        }
        Err(_) => panic!("test URL must be Unicode"),
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create guarded disposable test database; no credential fallback");
    Some(Fixture { db })
}

#[test]
fn fixture_refuses_unsafe_database_urls_without_connecting() {
    // No connection or DDL happens here: the shared guard refuses before any
    // pool is opened, covering unsafe authority, database, query/fragment,
    // and percent-escape smuggling.
    for raw in [
        "not a URL",
        "https://agent_test:@agent-testdb:5432/two_bot_test_guard",
        "postgres://agent_test:@production:5432/two_bot_test_guard",
        "postgres://agent_test:@agent-testdb:5433/two_bot_test_guard",
        "postgres://agent_test:@agent-testdb/two_bot_test_guard",
        "postgres://postgres:@agent-testdb:5432/two_bot_test_guard",
        "postgres://agent_test:secret@agent-testdb:5432/two_bot_test_guard",
        "postgres://agent_test@agent-testdb:5432/two_bot_test_guard",
        "postgres://agent_test:@agent-testdb:5432/postgres",
        "postgres://agent_test:@agent-testdb:5432/two_bot",
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_",
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_Guard",
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_guard/other",
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_%67uard",
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_guard?sslmode=disable",
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_guard#fragment",
    ] {
        assert!(
            two_bot_testsupport::guard_database_url(raw).is_err(),
            "accepted unsafe fixture: {raw}"
        );
    }
    two_bot_testsupport::guard_database_url(
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_guard",
    )
    .expect("safe bootstrap URL");
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
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = MockRest::start(
        vec![
            // Deferred edits require ID-bearing 200 receipts (executor mutation_receipt_id).
            // RA-01 order per RSVP: ack, live-membership GET, live-event GET.
            // RA-03 post-write fence re-reads membership + event before the edit.
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            member(USER),
            event(1),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            member(USER),
            event(2),
            member(USER),
            event(1),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            member(USER),
            event(3),
            member(USER),
            event(1),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            member(USER),
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
        run(fixture.pool(), &mock, &rsvp(100 + i as u64, status)).await;
        assert_reply(&mock, &format!("RSVP saved: {status}."), true);
        let stored: String = sqlx::query_scalar(
            "SELECT status FROM event_rsvps WHERE guild_id=$1 AND event_id=$2 AND user_id=$3",
        )
        .bind(GUILD)
        .bind(EVENT)
        .bind(USER)
        .fetch_one(fixture.pool())
        .await
        .unwrap();
        assert_eq!(&stored, status);
    }
    run(
        fixture.pool(),
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
    assert_eq!(counts(fixture.pool()).await, (1, 4, 0));
    let audits: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT actor_id, action, target_key, outcome FROM announcements_audit_log ORDER BY id",
    )
    .fetch_all(fixture.pool())
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
    assert_eq!(requests.len(), 26);
    assert_eq!(requests[24].method, "POST");
    assert_eq!(requests[25].method, "PATCH");
    for chunk in requests[..24].chunks(6) {
        let ack: Value = serde_json::from_slice(&chunk[0].body).unwrap();
        assert_eq!(ack["type"], 5);
        assert_eq!(ack["data"]["flags"], 64);
        assert_eq!(chunk[1].method, "GET");
        assert_eq!(
            chunk[1].path,
            format!("/api/v10/guilds/{GUILD}/members/{USER}")
        );
        assert_eq!(chunk[2].method, "GET");
        assert_eq!(
            chunk[2].path,
            format!("/api/v10/guilds/{GUILD}/scheduled-events/{EVENT}")
        );
        // RA-03 post-write fence re-reads the same evidence before the edit.
        assert_eq!(chunk[3].method, "GET");
        assert_eq!(
            chunk[3].path,
            format!("/api/v10/guilds/{GUILD}/members/{USER}")
        );
        assert_eq!(chunk[4].method, "GET");
        assert_eq!(
            chunk[4].path,
            format!("/api/v10/guilds/{GUILD}/scheduled-events/{EVENT}")
        );
    }
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn missing_cancelled_and_malformed_events_refuse_without_writes() {
    let Some(fixture) = pool().await else {
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
                // RA-01: the membership gate passes here, then the event lookup
                // refuses; both precede any mutation or audit write.
                member(USER),
                lookup,
                // Refusal text is still delivered as a deferred edit: ID receipt required.
                ScriptedResponse::json(200, json!({"id": "99"})),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        run(fixture.pool(), &mock, &rsvp(200, "going")).await;
        assert_reply(&mock, reply, true);
        assert_eq!(mock.requests().len(), 4);
        assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
        mock.shutdown().await;
    }
    fixture.close().await;
}

/// RA-01: a departed acting member (404 membership read) is refused before
/// the event lookup runs, with zero RSVP/audit writes.
#[tokio::test]
async fn rsvp_refuses_departed_member_before_event_lookup() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::status(404),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(fixture.pool(), &mock, &rsvp(210, "going")).await;
    assert_reply(&mock, "You are no longer a member of this server.", true);
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests
        .iter()
        .all(|r| !r.path.contains("scheduled-events")));
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;
    fixture.close().await;
}

/// RA-01: a failed membership lookup (denied, malformed evidence, or a
/// wrong-user echo) fails closed with zero writes, never treated as absence
/// or success.
#[tokio::test]
async fn rsvp_membership_lookup_failure_fails_closed() {
    let Some(fixture) = pool().await else {
        return;
    };
    for broken in [
        ScriptedResponse::status(403),
        ScriptedResponse {
            body: b"invalid-json".to_vec(),
            ..ScriptedResponse::status(200)
        },
        ScriptedResponse::json(200, json!({"user": {"id": OTHER}, "roles": []})),
    ] {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::status(204),
                broken,
                ScriptedResponse::json(200, json!({"id": "99"})),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        run(fixture.pool(), &mock, &rsvp(220, "going")).await;
        assert_reply(&mock, "Unable to verify server membership.", true);
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|r| !r.path.contains("scheduled-events")));
        assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
        mock.shutdown().await;
    }
    fixture.close().await;
}

#[tokio::test]
async fn totals_remain_readable_without_live_event_access() {
    let Some(fixture) = pool().await else {
        return;
    };
    two_bot_core::put_rsvp(
        fixture.pool(),
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
            fixture.pool(),
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
        assert_eq!(counts(fixture.pool()).await, (1, 0, 0));
        mock.shutdown().await;
    }
    fixture.close().await;
}

#[tokio::test]
async fn permissions_gates_and_guild_fence_precede_store_access() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    // RA-02 unauthorized self: the invoker selects themself with no Manage
    // Events. The router refuses before any callback deferral, store work or
    // network, so self-select opens no unprivileged path.
    run(
        fixture.pool(),
        &mock,
        &attendance(300, 0, USER, &anchored("2026-09-30")),
    )
    .await;
    assert_reply(&mock, "You need the Manage Events permission to use this command. Ask a server admin to grant it.", false);
    assert_eq!(mock.requests().len(), 1);
    // RA-02 unauthorized on-behalf: same refusal when selecting someone else.
    run(
        fixture.pool(),
        &mock,
        &attendance(304, 0, OTHER, &anchored("2026-09-30")),
    )
    .await;
    assert_reply(&mock, "You need the Manage Events permission to use this command. Ask a server admin to grant it.", false);
    assert_eq!(mock.requests().len(), 2);
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
            fixture.pool(),
            &executor(&mock),
            &ClassifierConfig::default(),
            &interaction,
        )
        .await
        .unwrap();
    }
    assert_reply(
        &mock,
        "Attendance capture is not enabled on this server. Ask a server admin to enable it in the bot configuration — this is a host setting, not a Discord role.",
        false,
    );
    let mut foreign = rsvp(303, "going");
    foreign.guild_id = Some(twilight_model::id::Id::new(999));
    assert!(!handle_rsvp_interaction(
        &router(),
        fixture.pool(),
        &executor(&mock),
        &ClassifierConfig::default(),
        &foreign
    )
    .await
    .unwrap());
    foreign.guild_id = None;
    assert!(!handle_rsvp_interaction(
        &router(),
        fixture.pool(),
        &executor(&mock),
        &ClassifierConfig::default(),
        &foreign
    )
    .await
    .unwrap());
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn host_checkin_authorized_self_and_on_behalf_retain_host_checkin_proof() {
    let Some(fixture) = pool().await else {
        return;
    };
    // RA-02 authorized paths: self-select (the invoker USER is also the
    // target) and on-behalf (OTHER) both pass the Manage Events gate, resolve
    // through the live guild event, verify live membership, and record with
    // `host_checkin` proof — self-select neither bypasses authority nor
    // downgrades the proof. Each check-in is one 204 callback, one live-event
    // GET, one live-member GET, and one ID-bearing 200 deferred edit, plus
    // the RA-03 post-write fence (one live-event re-read, one live-member
    // re-read) on every effective write. The duplicate check-in writes
    // nothing, so it revalidates nothing.
    let occ = anchored("2026-09-30");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            member_get(USER, None),
            event(1),
            member_get(USER, None),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            event(1),
            member_get(USER, None),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            event(1),
            member(USER),
            member_get(OTHER, Some(true)),
            event(1),
            member_get(OTHER, Some(true)),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            event(2),
            member_get(USER, None),
            event(2),
            member_get(USER, None),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(204),
    )
    .await;
    // Padded input canonicalizes to the anchored id in the reply and the fact.
    run(
        fixture.pool(),
        &mock,
        &attendance(400, MANAGE_EVENTS, USER, &format!(" {occ} ")),
    )
    .await;
    assert_reply(
        &mock,
        &format!("Recorded <@{USER}> for event occurrence `{occ}`."),
        true,
    );
    run(
        fixture.pool(),
        &mock,
        &attendance(401, MANAGE_EVENTS, USER, &occ),
    )
    .await;
    assert_reply(
        &mock,
        &format!("Attendance for <@{USER}> and event occurrence `{occ}` was already recorded."),
        true,
    );
    run(
        fixture.pool(),
        &mock,
        &attendance(402, MANAGE_EVENTS, OTHER, &occ),
    )
    .await;
    assert_eq!(counts(fixture.pool()).await, (0, 0, 2));
    let rows: Vec<(String, String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT event_type, source_event_id, actor_id, source, classification, metadata, idempotency_key FROM community_facts ORDER BY id")
        .fetch_all(fixture.pool()).await.unwrap();
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
    // A labeled repeat occurrence of a live event records as its own fact.
    let next = anchored("next");
    let mut config = ClassifierConfig::default();
    config.test_actor_ids.insert(USER.into());
    handle_rsvp_interaction(
        &router(),
        fixture.pool(),
        &executor(&mock),
        &config,
        &attendance(403, MANAGE_EVENTS, USER, &next),
    )
    .await
    .unwrap();
    let verdict: (String, String, String, String) = sqlx::query_as("SELECT classifier_version, classification, matched_rule, metadata FROM community_facts WHERE source_event_id = $1")
        .bind(format!("{next}:{USER}")).fetch_one(fixture.pool()).await.unwrap();
    assert_eq!(
        (verdict.0.as_str(), verdict.1.as_str(), verdict.2.as_str()),
        ("community-v1", "test", "configured_test_actor")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&verdict.3).unwrap(),
        json!({"eventOccurrenceId": next, "proof": "host_checkin"})
    );
    // Every check-in proved its occurrence against the live guild event and
    // its target against live membership — pre-write reads and the RA-03
    // post-write fence alike. The on-behalf check-in additionally proved the
    // acting host between the event and target reads (RA-01 host gate); self
    // check-ins skip that lookup. The duplicate wrote nothing, so only the
    // three effective writes carry fence re-reads.
    let requests = mock.requests();
    assert_eq!(requests.len(), 23);
    let gets: Vec<_> = requests.iter().filter(|r| r.method == "GET").collect();
    assert_eq!(gets.len(), 15);
    // Self check-in 400: event, target, fence event, fence target.
    assert_eq!(gets[0].path, scheduled_event_path(EVENT));
    assert_eq!(gets[1].path, guild_member_path(USER));
    assert_eq!(gets[2].path, scheduled_event_path(EVENT));
    assert_eq!(gets[3].path, guild_member_path(USER));
    // Duplicate 401: event, target; no fence on a write that did nothing.
    assert_eq!(gets[4].path, scheduled_event_path(EVENT));
    assert_eq!(gets[5].path, guild_member_path(USER));
    // On-behalf 402: event, host, target, fence event, fence target.
    assert_eq!(gets[6].path, scheduled_event_path(EVENT));
    assert_eq!(gets[7].path, guild_member_path(USER));
    assert_eq!(gets[8].path, guild_member_path(OTHER));
    assert_eq!(gets[9].path, scheduled_event_path(EVENT));
    assert_eq!(gets[10].path, guild_member_path(OTHER));
    // Labeled self check-in 403: event, target, fence event, fence target.
    assert_eq!(gets[11].path, scheduled_event_path(EVENT));
    assert_eq!(gets[12].path, guild_member_path(USER));
    assert_eq!(gets[13].path, scheduled_event_path(EVENT));
    assert_eq!(gets[14].path, guild_member_path(USER));
    assert_eq!(counts(fixture.pool()).await, (0, 0, 3));
    fixture.close().await;
    mock.shutdown().await;
}

/// RA-01 host gate on the RA-02 trusted path: a departed acting host, a
/// departed target, and failed membership lookups refuse with zero attendance
/// writes. Occurrences anchor to the live event first, so every case binds
/// the event and then fails at the membership read. A self check-in skips
/// the host lookup — the live target read verifies the same membership —
/// while on-behalf check-ins prove the host between the event and target
/// reads.
#[tokio::test]
async fn host_checkin_membership_gates_precede_attendance_writes() {
    let Some(fixture) = pool().await else {
        return;
    };
    let occ = anchored("2026-09-30");
    // Departed self host: event binds, then the live target read 404s and the
    // refusal edit follows. No host lookup runs for a self check-in.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            ScriptedResponse::status(404),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(
        fixture.pool(),
        &mock,
        &attendance(410, MANAGE_EVENTS, USER, &occ),
    )
    .await;
    assert_reply(&mock, "That member is not in this server.", true);
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;
    // Departed target: event binds, actor passes the host gate, target 404s,
    // then the refusal edit.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            member(USER),
            ScriptedResponse::status(404),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(
        fixture.pool(),
        &mock,
        &attendance(411, MANAGE_EVENTS, OTHER, &occ),
    )
    .await;
    assert_reply(&mock, "That member is not in this server.", true);
    assert_eq!(mock.requests().len(), 5);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;
    // Failed host lookup: malformed evidence fails closed with zero writes.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            ScriptedResponse {
                body: b"invalid-json".to_vec(),
                ..ScriptedResponse::status(200)
            },
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(
        fixture.pool(),
        &mock,
        &attendance(412, MANAGE_EVENTS, OTHER, &occ),
    )
    .await;
    assert_reply(&mock, "Unable to verify server membership.", true);
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;
    // Failed target lookup: wrong-user echo fails closed with zero writes.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            member(USER),
            ScriptedResponse::json(200, json!({"user": {"id": USER}, "roles": []})),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(
        fixture.pool(),
        &mock,
        &attendance(413, MANAGE_EVENTS, OTHER, &occ),
    )
    .await;
    assert_reply(&mock, "Unable to verify attendance member.", true);
    assert_eq!(mock.requests().len(), 5);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;
    fixture.close().await;
}

#[tokio::test]
async fn malformed_inputs_and_failed_ack_do_not_write() {
    let Some(fixture) = pool().await else {
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
    run(fixture.pool(), &mock, &rsvp(500, "bogus")).await;
    run(
        fixture.pool(),
        &mock,
        &attendance(501, MANAGE_EVENTS, USER, "  "),
    )
    .await;
    assert_reply(&mock, "event occurrence must not be empty.", true);
    // Forged user id with a well-formed anchored occurrence: the resolved-user
    // pre-check refuses before any event or member lookup hits the network.
    run(
        fixture.pool(),
        &mock,
        &attendance(
            502,
            MANAGE_EVENTS,
            "1546451670500642111",
            &anchored("2026-09-30"),
        ),
    )
    .await;
    assert_reply(&mock, "Unable to resolve attendance member.", true);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    assert_eq!(mock.requests().len(), 6);
    assert!(
        mock.requests().iter().all(|r| r.method != "GET"),
        "malformed and forged inputs refuse before any Discord lookup"
    );
    let denied = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    assert!(handle_rsvp_interaction(
        &router(),
        fixture.pool(),
        &executor(&denied),
        &ClassifierConfig::default(),
        &rsvp(503, "going")
    )
    .await
    .is_err());
    assert_eq!(denied.requests().len(), 1);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    fixture.close().await;
    denied.shutdown().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_and_cross_guild_occurrences_refuse_without_writes() {
    let Some(fixture) = pool().await else {
        return;
    };
    // RA-02 occurrence binding: unknown, foreign, cancelled and unreadable
    // anchors refuse, as do bare slugs with no anchor to look up. Every case
    // leaves RSVP, audit and attendance row counts at zero.
    const UNANCHORED: &str =
        "\"event occurrence\" must be a scheduled event id, optionally with a :label suffix";
    let anchored_unknown = format!("{OTHER}:2026-10-03");
    let cases: Vec<(String, Option<String>, Option<ScriptedResponse>, &str)> = vec![
        // Unknown snowflake event.
        (
            OTHER.to_owned(),
            Some(OTHER.to_owned()),
            Some(ScriptedResponse::status(404)),
            "No scheduled event with that id exists in this server.",
        ),
        // Anchored to an unknown event.
        (
            anchored_unknown,
            Some(OTHER.to_owned()),
            Some(ScriptedResponse::status(404)),
            "No scheduled event with that id exists in this server.",
        ),
        // Anchor lives in another guild.
        (
            EVENT.to_owned(),
            Some(EVENT.to_owned()),
            Some(ScriptedResponse::json(
                200,
                json!({"id": EVENT, "guild_id": "999", "status": 1}),
            )),
            "Discord returned an invalid scheduled event status.",
        ),
        // Anchor is cancelled.
        (
            EVENT.to_owned(),
            Some(EVENT.to_owned()),
            Some(event(4)),
            "That scheduled event is cancelled.",
        ),
        // Anchor has an unknown status.
        (
            EVENT.to_owned(),
            Some(EVENT.to_owned()),
            Some(event(9)),
            "Discord returned an invalid scheduled event status.",
        ),
        // Anchor lookup is unreadable: fail closed, not absent.
        (
            EVENT.to_owned(),
            Some(EVENT.to_owned()),
            Some(ScriptedResponse {
                body: b"invalid-json".to_vec(),
                ..ScriptedResponse::status(200)
            }),
            "Discord returned an invalid scheduled event status.",
        ),
        // Anchor lookup fails: fail closed with zero writes.
        (
            EVENT.to_owned(),
            Some(EVENT.to_owned()),
            Some(ScriptedResponse::status(500)),
            "Discord request failed: HTTP 500",
        ),
        // Bare slugs name no anchorable event: refuse before any lookup.
        (
            "weekly-standup-2026-10-03".to_owned(),
            None,
            None,
            UNANCHORED,
        ),
        (format!("{EVENT}:"), None, None, UNANCHORED),
        ("not-an-event:2026-10-03".to_owned(), None, None, UNANCHORED),
    ];
    for (i, (occurrence, anchor, lookup, reply)) in cases.into_iter().enumerate() {
        let mut script = vec![ScriptedResponse::status(204)];
        if let Some(lookup) = lookup {
            script.push(lookup);
        }
        script.push(ScriptedResponse::json(200, json!({"id": "99"})));
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        run(
            fixture.pool(),
            &mock,
            &attendance(600 + i as u64, MANAGE_EVENTS, USER, &occurrence),
        )
        .await;
        assert_reply(&mock, reply, true);
        let requests = mock.requests();
        match anchor {
            Some(anchor) => {
                assert_eq!(requests.len(), 3, "callback, lookup, edit");
                assert_eq!(requests[1].method, "GET");
                assert_eq!(requests[1].path, scheduled_event_path(&anchor));
            }
            None => {
                assert_eq!(requests.len(), 2, "callback and edit only");
                assert!(requests.iter().all(|r| r.method != "GET"));
            }
        }
        assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
        mock.shutdown().await;
    }
    fixture.close().await;
}

#[tokio::test]
async fn non_member_and_unverifiable_targets_refuse_without_writes() {
    let Some(fixture) = pool().await else {
        return;
    };
    // RA-02 target membership: the resolved User proves identity, not
    // membership. Departed, cross-guild and unverifiable targets refuse after
    // a successful occurrence binding, writing zero rows. The acting host
    // passes the RA-01 gate first, so every case carries its host lookup.
    let occ = anchored("2026-09-30");
    let cases: Vec<(ScriptedResponse, &str)> = vec![
        // Departed, never-joined or cross-guild target.
        (
            ScriptedResponse::status(404),
            "That member is not in this server.",
        ),
        // Live read names a different user: fail closed.
        (
            ScriptedResponse::json(200, json!({"user": {"id": USER}, "roles": []})),
            "Unable to verify attendance member.",
        ),
        // Malformed member document: fail closed.
        (
            ScriptedResponse::json(200, json!({"no_user": true})),
            "Unable to verify attendance member.",
        ),
        // Transport failure: fail closed without transport detail.
        (
            ScriptedResponse::status(500),
            "Unable to verify attendance member.",
        ),
    ];
    for (i, (member_lookup, reply)) in cases.into_iter().enumerate() {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::status(204),
                event(1),
                member(USER),
                member_lookup,
                ScriptedResponse::json(200, json!({"id": "99"})),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        run(
            fixture.pool(),
            &mock,
            &attendance(750 + i as u64, MANAGE_EVENTS, OTHER, &occ),
        )
        .await;
        assert_reply(&mock, reply, true);
        let requests = mock.requests();
        assert_eq!(
            requests.len(),
            5,
            "callback, event lookup, host lookup, member lookup, edit"
        );
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[1].method, "GET");
        assert_eq!(requests[1].path, scheduled_event_path(EVENT));
        assert_eq!(requests[2].method, "GET");
        assert_eq!(requests[2].path, guild_member_path(USER));
        assert_eq!(requests[3].method, "GET");
        assert_eq!(requests[3].path, guild_member_path(OTHER));
        assert_eq!(requests[4].method, "PATCH");
        assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
        mock.shutdown().await;
    }
    fixture.close().await;
}

#[tokio::test]
async fn failed_final_reply_does_not_retry_committed_effects() {
    let Some(fixture) = pool().await else {
        return;
    };
    // Anchored check-in occurrence: the trusted path still commits the fact
    // before the reply edit, so a failed edit must not double-record.
    let occ = anchored("weekly");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            // RA-01 membership GET precedes the live-event lookup on mutations.
            member(USER),
            event(1),
            // RA-03 post-write fence re-reads membership + event before the edit.
            member(USER),
            event(1),
            ScriptedResponse::status(500),
            // Discord refuses a repeated acknowledgement for the same interaction.
            ScriptedResponse::status(400),
            ScriptedResponse::status(204),
            event(1),
            member_get(USER, None),
            // RA-03 post-write fence re-reads the live event + membership
            // before the edit; the failed edit still leaves one fact.
            event(1),
            member_get(USER, None),
            ScriptedResponse::status(500),
            ScriptedResponse::status(204),
            event(1),
            member_get(USER, None),
            // Intended successful duplicate check-in edit: ID receipt required.
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(fixture.pool(), &mock);
    let interaction = rsvp(600, "going");
    assert!(runtime.handle(&interaction).await.is_err());
    assert_eq!(counts(fixture.pool()).await, (1, 1, 0));
    assert_eq!(mock.requests().len(), 6);
    assert!(runtime.handle(&interaction).await.is_err());
    assert_eq!(counts(fixture.pool()).await, (1, 1, 0));
    assert_eq!(mock.requests().len(), 7);

    assert!(runtime
        .handle(&attendance(601, MANAGE_EVENTS, USER, &occ))
        .await
        .is_err());
    assert_eq!(counts(fixture.pool()).await, (1, 1, 1));
    assert_eq!(mock.requests().len(), 13);
    assert!(runtime
        .handle(&attendance(602, MANAGE_EVENTS, USER, &occ))
        .await
        .unwrap());
    assert_reply(
        &mock,
        &two_bot_core::checkin_duplicate_text(USER, &occ),
        true,
    );
    assert_eq!(counts(fixture.pool()).await, (1, 1, 1));
    fixture.close().await;
    mock.shutdown().await;
}

/// RA-03: the event vanishing between lookup and commit is refused and
/// compensated — the success reply never fires and no rows remain.
#[tokio::test]
async fn rsvp_mid_write_event_removal_is_compensated_and_refused() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            // Post-write fence: membership still current, event now gone.
            member(USER),
            ScriptedResponse::status(404),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(fixture.pool(), &mock, &rsvp(800, "going")).await;
    assert_reply(
        &mock,
        "No scheduled event with that id exists in this server.",
        true,
    );
    assert_eq!(mock.requests().len(), 6);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;
    fixture.close().await;
}

/// RA-03: the actor departing between lookup and commit is refused and
/// compensated. The fence short-circuits on the lost membership, so no
/// second event re-read runs.
#[tokio::test]
async fn rsvp_mid_write_membership_loss_is_compensated_and_refused() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            // Post-write fence: the actor is gone.
            ScriptedResponse::status(404),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(fixture.pool(), &mock, &rsvp(801, "going")).await;
    assert_reply(&mock, "You are no longer a member of this server.", true);
    assert_eq!(mock.requests().len(), 5);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;
    fixture.close().await;
}

/// RA-03: the check-in target departing between lookup and commit is
/// refused and compensated — no attendance fact survives.
#[tokio::test]
async fn checkin_mid_write_target_departure_is_compensated_and_refused() {
    let Some(fixture) = pool().await else {
        return;
    };
    let occ = anchored("2026-09-30");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            // RA-01 host gate: on-behalf target, so the acting host is read
            // between the event and target lookups.
            member(USER),
            member_get(OTHER, None),
            // Post-write fence: the anchor is still live, the target now gone.
            event(1),
            ScriptedResponse::status(404),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(
        fixture.pool(),
        &mock,
        &attendance(802, MANAGE_EVENTS, OTHER, &occ),
    )
    .await;
    assert_reply(&mock, "That member is no longer in this server.", true);
    assert_eq!(mock.requests().len(), 7);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;
    fixture.close().await;
}

/// RA-03: store failures fail closed — a refused commit is reported as a
/// refusal, never as a saved RSVP or a recorded check-in. The missing tables
/// stand in for an unavailable store; the audit table survives to prove the
/// refusal wrote nothing.
#[tokio::test]
async fn store_failures_never_report_success() {
    let Some(fixture) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE event_rsvps")
        .execute(fixture.pool())
        .await
        .unwrap();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(fixture.pool(), &mock, &rsvp(803, "going")).await;
    assert_reply(&mock, "Unable to save RSVP.", true);
    assert_eq!(mock.requests().len(), 4);
    let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM announcements_audit_log")
        .fetch_one(fixture.pool())
        .await
        .unwrap();
    assert_eq!(audits, 0);
    mock.shutdown().await;
    fixture.close().await;

    let Some(second) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE community_facts")
        .execute(second.pool())
        .await
        .unwrap();
    let occ = anchored("2026-09-30");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            member_get(USER, None),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(
        second.pool(),
        &mock,
        &attendance(804, MANAGE_EVENTS, USER, &occ),
    )
    .await;
    assert_reply(&mock, "Attendance was not recorded.", true);
    assert_eq!(mock.requests().len(), 4);
    let rsvps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_rsvps")
        .fetch_one(second.pool())
        .await
        .unwrap();
    assert_eq!(rsvps, 0);
    mock.shutdown().await;
    second.close().await;
}

/// RA-03: a full event refuses a new member through the live path with no
/// new rows, and a member who spent their per-minute budget is refused with
/// no extra rows — the refusal texts name the bound.
#[tokio::test]
async fn rsvp_capacity_and_rate_refuse_without_extra_rows() {
    let Some(fixture) = pool().await else {
        return;
    };
    // Fill the event at the store layer: 1,000 distinct members, no audits,
    // so the per-user rate ledger stays empty for the live attempt.
    for i in 0..1_000 {
        two_bot_core::put_rsvp(
            fixture.pool(),
            &two_bot_core::RsvpRecord {
                guild_id: GUILD.into(),
                event_id: EVENT.into(),
                user_id: format!("cap-{i:04}"),
                status: two_bot_core::RsvpStatus::Going,
                responded_at: format!("2026-09-10T10:{:02}:{:02}.000Z", i / 60, i % 60),
            },
        )
        .await
        .unwrap();
    }
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    // Pre-write fence passes, then the store refuses the 1,001st member;
    // the post-write fence never runs, so no re-read GETs follow.
    run(fixture.pool(), &mock, &rsvp(805, "going")).await;
    assert_reply(&mock, &two_bot_core::rsvp_event_full_text(), true);
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(counts(fixture.pool()).await, (1000, 0, 0));
    mock.shutdown().await;
    fixture.close().await;

    let Some(second) = pool().await else {
        return;
    };
    // Ledger a full minute budget for USER at the store layer. The live
    // attempt below stamps `now_iso()`, so the ledger must cover the minute
    // that attempt lands in: three consecutive minutes starting with the
    // current one (ledgering takes seconds, so the attempt cannot escape).
    // 20 writes per minute collapse onto one RSVP row plus 60 audit rows.
    let epoch_ms = two_bot_core::funnel::now_millis_for_test();
    let base_minute = epoch_ms - epoch_ms.rem_euclid(60_000);
    for m in 0..3 {
        for i in 0..20 {
            let at = two_bot_core::format_iso_millis(base_minute + m * 60_000 + i * 1_000);
            let record = two_bot_core::RsvpRecord {
                guild_id: GUILD.into(),
                event_id: EVENT.into(),
                user_id: USER.into(),
                status: two_bot_core::RsvpStatus::Going,
                responded_at: at,
            };
            two_bot_core::put_rsvp(second.pool(), &record)
                .await
                .unwrap();
            two_bot_core::write_audit(
                second.pool(),
                &two_bot_core::RsvpAudit::for_rsvp(&format!("rate-{m}-{i}"), &record),
            )
            .await
            .unwrap();
        }
    }
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(second.pool(), &mock, &rsvp(806, "going")).await;
    assert_reply(&mock, &two_bot_core::rsvp_rate_limited_text(), true);
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(counts(second.pool()).await, (1, 60, 0));
    mock.shutdown().await;
    second.close().await;
}

/// RA-03: a full occurrence refuses a new check-in through the live path
/// with no new facts.
#[tokio::test]
async fn checkin_at_capacity_refuses_without_new_facts() {
    let Some(fixture) = pool().await else {
        return;
    };
    // The live attempt records under the RA-02 canonical id, so the
    // pre-fill uses that same string: an anchored occurrence of the live event.
    let occ = anchored("full");
    let human = two_bot_core::checkin_classification(false);
    for i in 0..1_000 {
        assert!(
            two_bot_core::record_checkin(
                fixture.pool(),
                &two_bot_core::CheckinWrite {
                    guild_id: GUILD.into(),
                    event_occurrence_id: occ.clone(),
                    member_id: format!("member-{i:04}"),
                    occurred_at: "2026-09-02T10:00:00.000Z".into(),
                    proof: two_bot_core::AttendanceProof::HostCheckin,
                    classifier_version: "community-test-v1".into(),
                    classification: human.classification.into(),
                    matched_rule: human.matched_rule.into(),
                },
            )
            .await
            .unwrap(),
            "member {i} admitted"
        );
    }
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            member_get(USER, None),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(
        fixture.pool(),
        &mock,
        &attendance(807, MANAGE_EVENTS, USER, &occ),
    )
    .await;
    assert_reply(&mock, &two_bot_core::checkin_occurrence_full_text(), true);
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 1000));
    mock.shutdown().await;
    fixture.close().await;
}

#[tokio::test]
async fn shared_runtime_publishes_complete_registry_once() {
    let Some(fixture) = pool().await else {
        return;
    };
    // Bare 200 triggers the mock's guild-command PUT echo self-heal, returning the
    // complete submitted command list as the registry receipt.
    let mock = MockRest::start(vec![], ScriptedResponse::status(200)).await;
    runtime(fixture.pool(), &mock).publish(1111).await.unwrap();
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
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    fixture.close().await;
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
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(fixture.pool(), &mock);
    runtime.set_application_id(2222);
    let mut foreign = rsvp(700, "going");
    foreign.application_id = twilight_model::id::Id::new(9999);
    // Same fence as `handle`/`handle_routed`: the split prepare path refuses
    // before any callback or store work, and complete stays a no-op.
    let prepared = runtime.prepare(foreign).await.expect("prepare refusal");
    assert!(!runtime.complete(prepared).await.expect("complete refusal"));
    assert!(mock.requests().is_empty());
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    fixture.close().await;
    mock.shutdown().await;
}
