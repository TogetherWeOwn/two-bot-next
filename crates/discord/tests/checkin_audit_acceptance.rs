//! Acceptance: every recorded check-in leaves the same audit trace as an
//! RSVP. Disposable-DB tests in the RSVP harness shape; never live Discord.
//!
//! The audit-row contract these tests pin, mirroring the RSVP row asserted
//! in `rsvp_runtime.rs` (`actor_id`, `action = "event.rsvp"`,
//! `target_key`, `outcome = <status>`, id `rsvp:<interaction id>`):
//!
//! - one `announcements_audit_log` row per recorded (non-duplicate)
//!   check-in, none for a duplicate and none on any refusal path;
//! - `id = "checkin:<interaction id>"` (the RSVP idempotency pattern);
//! - `actor_id` is the acting host (the invoker), including on-behalf;
//! - `action = "event.checkin"`;
//! - `target_key = "<occurrence>:<member>"`, the attendance fact's
//!   `source_event_id`;
//! - `outcome = "recorded"`.
//!
//! When the audit write dedupes on that id (the row already exists), the
//! check-in still succeeds and the dedupe is observable as exactly one
//! DEBUG tracing event with `msg = "checkin_audit_duplicate"`; silently
//! discarding the `write_audit` boolean fails this suite.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{Pool, Postgres};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
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

fn attendance(id: u64, permissions: u64, member: &str, occurrence: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": id.to_string(), "token": "mock-checkin-token", "type": 2,
        "version": 1, "guild_id": GUILD,
        "member": {"permissions": permissions.to_string(), "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": USER, "username": "invoker", "discriminator": "0"}},
        "data": {"id": "4444", "name": "attendance", "type": 1, "options": [
            {"name": "event-occurrence", "type": 3, "value": occurrence},
            {"name": "member", "type": 6, "value": member}
        ],
            "resolved": {"users": {
                USER: {"id": USER, "username": "human", "discriminator": "0"},
                OTHER: {"id": OTHER, "username": "bot", "discriminator": "0", "bot": true}
            }}}
    }))
    .expect("wire interaction")
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

/// RA-02 anchored occurrence: the live event plus a repeat label.
fn anchored(label: &str) -> String {
    format!("{EVENT}:{label}")
}

/// Live `GET /guilds/{g}/members/{u}` read: `bot: None` omits the key
/// (Discord omits it for humans), exercising the default-false path.
fn member_get(user: &str, bot: Option<bool>) -> ScriptedResponse {
    let mut member = json!({"id": user});
    if let Some(bot) = bot {
        member["bot"] = json!(bot);
    }
    ScriptedResponse::json(200, json!({"user": member, "roles": []}))
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy(
        "mock-token-not-a-credential".to_owned(),
        Some(mock.origin()),
    )
    .unwrap()
}

/// Guarded disposable fixture; each test gets its own migrated database and
/// `close` drops it. Same guard and opt-in URL as the RSVP harness.
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

/// (event_rsvps, announcements_audit_log, community_facts) row counts, in
/// the RSVP harness's order: audit parity reads as the middle number
/// tracking the attendance number on every recorded check-in.
async fn counts(pool: &Pool<Postgres>) -> (i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM event_rsvps), (SELECT count(*) FROM announcements_audit_log), (SELECT count(*) FROM community_facts)")
        .fetch_one(pool).await.unwrap()
}

/// Every audit row, ordered by id (ids carry the interaction id, so id
/// order is attempt order within a test):
/// (id, guild_id, actor_id, action, target_key, outcome).
type AuditRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
);

async fn audit_rows(pool: &Pool<Postgres>) -> Vec<AuditRow> {
    sqlx::query_as(
        "SELECT id, guild_id, actor_id, action, target_key, outcome \
         FROM announcements_audit_log ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Process-wide JSON log sink, installed once for the binary at DEBUG so
/// the audit-dedupe debug signal is observable. Readers only consume
/// bytes written after their own mark.
#[derive(Clone, Default)]
struct LogSink(Arc<Mutex<Vec<u8>>>);

impl Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn log_sink() -> LogSink {
    static SINK: OnceLock<LogSink> = OnceLock::new();
    SINK.get_or_init(|| {
        let sink = LogSink::default();
        let writer = sink.clone();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(move || writer.clone())
                .finish(),
        )
        .expect("install the process-wide log sink once");
        sink
    })
    .clone()
}

/// Serializes the test that reads a diagnostic window, so no other test's
/// dedupe signal can land inside its window.
static DIAGNOSTIC_WINDOW: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn log_mark() -> usize {
    log_sink().0.lock().unwrap().len()
}

fn dedupe_signals_since(mark: usize) -> Vec<Value> {
    let bytes = log_sink().0.lock().unwrap()[mark..].to_vec();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["fields"]["msg"] == "checkin_audit_duplicate")
        .collect()
}

fn assert_reply(mock: &MockRest, content: &str, deferred: bool) {
    let requests = mock.requests();
    let last = requests.last().unwrap();
    let body: Value = serde_json::from_slice(&last.body).unwrap();
    if deferred {
        assert_eq!(last.method, "PATCH");
        assert_eq!(
            last.path,
            "/api/v10/webhooks/1111/mock-checkin-token/messages/@original"
        );
        assert_eq!(body["content"], content);
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

/// One recorded check-in = one attendance fact + one audit row, for a self
/// check-in and an on-behalf check-in alike; a duplicate adds neither.
#[tokio::test]
async fn recorded_checkins_write_one_audit_row_each_and_duplicates_none() {
    let Some(fixture) = pool().await else {
        return;
    };
    // Per effective write: 204 ack, live-event GET, live-member GET
    // (on-behalf also reads the acting host first), RA-03 fence re-reads,
    // ID-bearing 200 deferred edit. The duplicate writes nothing, so it
    // carries no fence re-reads.
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
        ],
        ScriptedResponse::status(500),
    )
    .await;

    // Recorded self check-in: 1 attendance + 1 audit row.
    run(
        fixture.pool(),
        &mock,
        &attendance(900, MANAGE_EVENTS, USER, &occ),
    )
    .await;
    assert_reply(
        &mock,
        &format!("Recorded <@{USER}> for event occurrence `{occ}`."),
        true,
    );
    assert_eq!(counts(fixture.pool()).await, (0, 1, 1));
    assert_eq!(
        audit_rows(fixture.pool()).await,
        [(
            "checkin:900".to_owned(),
            GUILD.to_owned(),
            Some(USER.to_owned()),
            "event.checkin".to_owned(),
            Some(format!("{occ}:{USER}")),
            "recorded".to_owned(),
        )]
    );

    // Duplicate: dedupe text, still 1 attendance, no new audit row — even
    // though this attempt's interaction id would mint a fresh audit id.
    run(
        fixture.pool(),
        &mock,
        &attendance(901, MANAGE_EVENTS, USER, &occ),
    )
    .await;
    assert_reply(
        &mock,
        &format!("Attendance for <@{USER}> and event occurrence `{occ}` was already recorded."),
        true,
    );
    assert_eq!(counts(fixture.pool()).await, (0, 1, 1));

    // Recorded on-behalf check-in: the acting host is the audit actor and
    // the checked-in member is named in the target key. (Reply copy is
    // pinned by the RSVP harness and out of scope here.)
    run(
        fixture.pool(),
        &mock,
        &attendance(902, MANAGE_EVENTS, OTHER, &occ),
    )
    .await;
    assert_eq!(counts(fixture.pool()).await, (0, 2, 2));
    assert_eq!(
        audit_rows(fixture.pool()).await,
        [
            (
                "checkin:900".to_owned(),
                GUILD.to_owned(),
                Some(USER.to_owned()),
                "event.checkin".to_owned(),
                Some(format!("{occ}:{USER}")),
                "recorded".to_owned(),
            ),
            (
                "checkin:902".to_owned(),
                GUILD.to_owned(),
                Some(USER.to_owned()),
                "event.checkin".to_owned(),
                Some(format!("{occ}:{OTHER}")),
                "recorded".to_owned(),
            ),
        ]
    );
    fixture.close().await;
    mock.shutdown().await;
}

/// Every check-in refusal path leaves zero attendance and zero audit rows:
/// permission gate, malformed input, occurrence binding, host and target
/// membership gates, and the compensated mid-write departure.
#[tokio::test]
async fn checkin_refusals_write_no_attendance_or_audit_rows() {
    let Some(fixture) = pool().await else {
        return;
    };
    let occ = anchored("2026-09-30");

    // No Manage Events: refused before deferral, self and on-behalf alike.
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    run(fixture.pool(), &mock, &attendance(910, 0, USER, &occ)).await;
    assert_reply(&mock, "You need the Manage Events permission to use this command. Ask a server admin to grant it.", false);
    run(fixture.pool(), &mock, &attendance(911, 0, OTHER, &occ)).await;
    assert_reply(&mock, "You need the Manage Events permission to use this command. Ask a server admin to grant it.", false);
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;

    // Malformed inputs refuse before any Discord lookup.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(
        fixture.pool(),
        &mock,
        &attendance(912, MANAGE_EVENTS, USER, "  "),
    )
    .await;
    assert_reply(&mock, "event occurrence must not be empty.", true);
    run(
        fixture.pool(),
        &mock,
        &attendance(913, MANAGE_EVENTS, USER, "weekly-standup-2026-10-03"),
    )
    .await;
    assert_reply(
        &mock,
        "\"event occurrence\" must be a scheduled event id, optionally with a :label suffix",
        true,
    );
    // Forged member id: the resolved-user pre-check refuses up front.
    run(
        fixture.pool(),
        &mock,
        &attendance(914, MANAGE_EVENTS, "1546451670500642111", &occ),
    )
    .await;
    assert_reply(&mock, "Unable to resolve attendance member.", true);
    assert_eq!(mock.requests().len(), 6);
    assert!(mock.requests().iter().all(|r| r.method != "GET"));
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;

    // Occurrence binding failures: unknown, cancelled, cross-guild and
    // unreadable anchors refuse after the live-event read.
    for (i, (lookup, reply)) in [
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
            ScriptedResponse::status(500),
            "Discord request failed: HTTP 500",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::status(204),
                lookup,
                ScriptedResponse::json(200, json!({"id": "99"})),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        run(
            fixture.pool(),
            &mock,
            &attendance(915 + i as u64, MANAGE_EVENTS, USER, &occ),
        )
        .await;
        assert_reply(&mock, reply, true);
        assert_eq!(mock.requests().len(), 3);
        assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
        mock.shutdown().await;
    }

    // Departed self target: the live member read 404s.
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
        &attendance(920, MANAGE_EVENTS, USER, &occ),
    )
    .await;
    assert_reply(&mock, "That member is not in this server.", true);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;

    // On-behalf membership gates: failed host lookup, departed target,
    // wrong-user target echo, malformed target document, failed target
    // lookup. Each case is event read, host read (the case's failing read
    // for the host case), then the refusal edit.
    let host_failure = MockRest::start(
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
        &host_failure,
        &attendance(921, MANAGE_EVENTS, OTHER, &occ),
    )
    .await;
    assert_reply(&host_failure, "Unable to verify server membership.", true);
    assert_eq!(host_failure.requests().len(), 4);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    host_failure.shutdown().await;

    for (i, (target_lookup, reply)) in [
        (
            ScriptedResponse::status(404),
            "That member is not in this server.",
        ),
        (
            ScriptedResponse::json(200, json!({"user": {"id": USER}, "roles": []})),
            "Unable to verify attendance member.",
        ),
        (
            ScriptedResponse::json(200, json!({"no_user": true})),
            "Unable to verify attendance member.",
        ),
        (
            ScriptedResponse::status(500),
            "Unable to verify attendance member.",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::status(204),
                event(1),
                member(USER),
                target_lookup,
                ScriptedResponse::json(200, json!({"id": "99"})),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        run(
            fixture.pool(),
            &mock,
            &attendance(922 + i as u64, MANAGE_EVENTS, OTHER, &occ),
        )
        .await;
        assert_reply(&mock, reply, true);
        assert_eq!(mock.requests().len(), 5);
        assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
        mock.shutdown().await;
    }

    // Mid-write departure: the post-write fence finds the target gone, the
    // attendance write is compensated and refused — and no audit row for
    // the refused attempt may remain either.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            member(USER),
            member_get(OTHER, None),
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
        &attendance(926, MANAGE_EVENTS, OTHER, &occ),
    )
    .await;
    assert_reply(&mock, "That member is no longer in this server.", true);
    assert_eq!(mock.requests().len(), 7);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 0));
    mock.shutdown().await;

    fixture.close().await;
}

/// A full occurrence refuses a new check-in with no new attendance fact
/// and no audit row.
#[tokio::test]
async fn checkin_at_capacity_writes_no_audit_row() {
    let Some(fixture) = pool().await else {
        return;
    };
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
    assert_eq!(counts(fixture.pool()).await, (0, 0, 1000));
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
        &attendance(927, MANAGE_EVENTS, USER, &occ),
    )
    .await;
    assert_reply(&mock, &two_bot_core::checkin_occurrence_full_text(), true);
    assert_eq!(mock.requests().len(), 4);
    assert_eq!(counts(fixture.pool()).await, (0, 0, 1000));
    mock.shutdown().await;
    fixture.close().await;
}

/// Audit idempotency dedupe: when this interaction's audit row already
/// exists, the recorded check-in still succeeds, no duplicate audit row
/// appears, and exactly one DEBUG dedupe signal names the collision
/// instead of the boolean being silently discarded.
#[tokio::test]
async fn checkin_audit_idempotency_dedupe_emits_debug_signal_without_duplicate_row() {
    let _window = DIAGNOSTIC_WINDOW.lock().await;
    let Some(fixture) = pool().await else {
        return;
    };
    let occ = anchored("2026-09-30");
    // A prior delivery of interaction 930 already wrote its audit row.
    sqlx::query(
        "INSERT INTO announcements_audit_log \
         (id, guild_id, actor_id, action, target_key, outcome, created_at) \
         VALUES ('checkin:930', $1, $2, 'event.checkin', $3, 'recorded', now())",
    )
    .bind(GUILD)
    .bind(USER)
    .bind(format!("{occ}:{USER}"))
    .execute(fixture.pool())
    .await
    .unwrap();
    assert_eq!(counts(fixture.pool()).await, (0, 1, 0));

    let mark = log_mark();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            member_get(USER, None),
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
        &attendance(930, MANAGE_EVENTS, USER, &occ),
    )
    .await;
    assert_reply(
        &mock,
        &format!("Recorded <@{USER}> for event occurrence `{occ}`."),
        true,
    );
    // Attendance recorded once; the audit table still holds only the
    // pre-existing row for this interaction.
    assert_eq!(counts(fixture.pool()).await, (0, 1, 1));
    assert_eq!(
        audit_rows(fixture.pool()).await,
        [(
            "checkin:930".to_owned(),
            GUILD.to_owned(),
            Some(USER.to_owned()),
            "event.checkin".to_owned(),
            Some(format!("{occ}:{USER}")),
            "recorded".to_owned(),
        )]
    );
    let signals = dedupe_signals_since(mark);
    let [signal] = signals.as_slice() else {
        panic!("expected one checkin_audit_duplicate signal, found {signals:?}");
    };
    assert_eq!(signal["level"], "DEBUG", "{signal}");
    assert!(
        !signal.to_string().contains("mock-checkin-token"),
        "dedupe signal leaked the interaction token: {signal}"
    );
    mock.shutdown().await;
    fixture.close().await;
}
