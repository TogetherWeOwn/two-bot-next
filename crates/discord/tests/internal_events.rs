//! Wire fixtures ported from legacy e2e.internalactions.test.ts (eventInput,
//! upsert/cancel/read) and unit.eventcancel.test.ts (status classification).
//! DB acceptance is opt-in, guarded, isolated by schema, and mandatory in CI.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)]
mod common;

use std::collections::BTreeMap;
use std::sync::Mutex;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use two_bot_core::internal_actions::{validate_event_input, EventInput, EventPlace};
use two_bot_core::{EventStatus, ScheduledEvent, ScheduledEventMirror};
use two_bot_discord::ratelimit_guard::GuardError;
use two_bot_discord::{
    event_status_name, is_definitive_rejection, scheduled_event_body, ActionExecutor, DiscordError,
    EventActionError, EventCall,
};

const GUILD: &str = "100000000000000001";
const EVENT: &str = "100000000000000002";
const CHANNEL: &str = "100000000000000003";
const OBSERVED: &str = "2026-09-01T18:00:00.000Z";

/// Fixed clock for the executor's post-return stamp: offline tests assert
/// exact mirror rows, while production passes `two_bot_core::now_iso`.
fn stamp() -> String {
    OBSERVED.to_owned()
}

fn input(voice: bool) -> EventInput {
    let mut body = json!({
        "name": "Launch Night",
        "starts_at": "2026-09-01T19:00:00Z",
        "ends_at": "2026-09-01T22:00:00Z",
    });
    if voice {
        body["channel_key"] = json!("lobby");
        body["description"] = json!("Meet in voice");
    } else {
        body["location"] = json!("The Together We Own server");
    }
    validate_event_input(
        body.as_object().unwrap(),
        &[("lobby".to_owned(), CHANNEL.to_owned())].into(),
    )
    .unwrap()
}

fn response(status: i64, voice: bool) -> Value {
    json!({
        "id": EVENT,
        "guild_id": GUILD,
        "name": "Launch Night",
        "scheduled_start_time": "2026-09-01T20:00:00+01:00",
        "channel_id": if voice { Some(CHANNEL) } else { None },
        "description": if voice { Some("Meet in voice") } else { None },
        "entity_metadata": if voice { Value::Null } else { json!({"location": "The Together We Own server"}) },
        "status": status,
        "user_count": 5,
        "creator": {"id": "100000000000000099"},
    })
}

fn golden() -> Value {
    serde_json::from_str(include_str!("fixtures/internal_event_bodies.json")).unwrap()
}

type MirrorRows = BTreeMap<(String, String), (ScheduledEvent, String)>;

#[derive(Default)]
struct MemoryMirror {
    rows: Mutex<MirrorRows>,
    fail: bool,
}

impl ScheduledEventMirror for MemoryMirror {
    async fn upsert(
        &self,
        guild_id: &str,
        observed_at: &str,
        event: &ScheduledEvent,
    ) -> Result<(), String> {
        if self.fail {
            return Err("scripted write failure".to_owned());
        }
        self.rows.lock().unwrap().insert(
            (guild_id.to_owned(), event.id.clone()),
            (event.clone(), observed_at.to_owned()),
        );
        Ok(())
    }
}

impl MemoryMirror {
    fn row(&self) -> Option<(ScheduledEvent, String)> {
        self.rows
            .lock()
            .unwrap()
            .get(&(GUILD.to_owned(), EVENT.to_owned()))
            .cloned()
    }
}

fn executor(rest: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(rest.origin())).unwrap()
}

#[test]
fn body_and_status_golden_contracts() {
    assert_eq!(
        scheduled_event_body(&input(false)),
        golden()["create_external"]
    );
    assert_eq!(scheduled_event_body(&input(true)), golden()["update_voice"]);
    for (status, expected) in [
        (1, "SCHEDULED"),
        (2, "ACTIVE"),
        (3, "COMPLETED"),
        (4, "CANCELED"),
        (99, "99"),
    ] {
        assert_eq!(event_status_name(&json!(status)).as_deref(), Some(expected));
    }
    for status in [Value::Null, json!("4"), json!(true), json!({})] {
        assert_eq!(event_status_name(&status), None);
    }
}

#[test]
fn local_guard_refusals_are_unsent_and_never_discord_rejections() {
    let fatal = EventActionError::Discord(DiscordError::Guard(GuardError::TokenInvalid));
    let wire = fatal.action_error();
    assert_eq!(wire.code.as_str(), "discord_unavailable");
    assert_eq!(wire.log_reason, "discord_guard_refused");
    assert_eq!(wire.retry_after_secs, None);
    assert!(fatal.is_safe_pre_mutation());

    for refusal in [
        GuardError::CircuitOpen,
        GuardError::AdmissionTimeout,
        GuardError::GlobalPaused,
    ] {
        let error = EventActionError::Discord(DiscordError::Guard(refusal));
        let wire = error.action_error();
        assert_eq!(wire.code.as_str(), "rate_limited");
        assert_eq!(wire.log_reason, "discord_guard_refused");
        assert_eq!(wire.retry_after_secs, Some(1));
        assert!(error.is_safe_pre_mutation());
    }
}

#[tokio::test]
async fn create_update_cancel_and_read_refresh_before_acknowledging() {
    let rest = MockRest::start(
        vec![
            ScriptedResponse::json(201, response(1, false)),
            ScriptedResponse::json(200, response(1, true)),
            ScriptedResponse::json(200, response(4, true)),
            ScriptedResponse::json(200, response(4, true)),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let executor = executor(&rest);
    let mirror = MemoryMirror::default();
    let created = executor
        .execute_event(
            GUILD,
            &EventCall::Upsert {
                event_id: None,
                input: input(false),
            },
            &mirror,
            stamp,
        )
        .await
        .unwrap();
    assert_eq!(created, json!({"outcome": "created", "event_id": EVENT}));
    let (row, observed) = mirror.row().unwrap();
    assert_eq!(row.starts_at, "2026-09-01T19:00:00.000Z");
    assert_eq!(row.channel_id, None);
    assert_eq!(row.description, None);
    assert_eq!(row.status, EventStatus::Scheduled);
    assert_eq!(observed, OBSERVED);
    let updated = executor
        .execute_event(
            GUILD,
            &EventCall::Upsert {
                event_id: Some(EVENT.to_owned()),
                input: input(true),
            },
            &mirror,
            stamp,
        )
        .await
        .unwrap();
    assert_eq!(updated, json!({"outcome": "updated", "event_id": EVENT}));
    assert_eq!(mirror.row().unwrap().0.channel_id.as_deref(), Some(CHANNEL));
    assert_eq!(
        mirror.row().unwrap().0.description.as_deref(),
        Some("Meet in voice")
    );
    let cancelled = executor
        .execute_event(
            GUILD,
            &EventCall::Cancel {
                event_id: EVENT.to_owned(),
            },
            &mirror,
            stamp,
        )
        .await
        .unwrap();
    assert_eq!(
        cancelled,
        json!({"outcome": "cancelled", "event_id": EVENT})
    );
    assert_eq!(mirror.row().unwrap().0.status, EventStatus::Cancelled);
    let read = executor
        .execute_event(
            GUILD,
            &EventCall::Read {
                event_id: EVENT.to_owned(),
            },
            &mirror,
            stamp,
        )
        .await
        .unwrap();
    assert_eq!(
        read,
        json!({
            "outcome": "read", "event_id": EVENT, "name": "Launch Night",
            "starts_at": "2026-09-01T20:00:00+01:00", "location": null,
            "status": "CANCELED", "observed_at": OBSERVED,
        })
    );
    let requests = rest.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].path,
        format!("/api/v10/guilds/{GUILD}/scheduled-events")
    );
    for index in [1, 2] {
        assert_eq!(requests[index].method, "PATCH");
        assert_eq!(
            requests[index].path,
            format!("/api/v10/guilds/{GUILD}/scheduled-events/{EVENT}")
        );
    }
    for (index, key) in [(0, "create_external"), (1, "update_voice"), (2, "cancel")] {
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[index].body).unwrap(),
            golden()[key]
        );
    }
    assert_eq!(requests[3].method, "GET");
    assert!(requests[3]
        .path
        .starts_with(&format!("/api/v10/guilds/{GUILD}/scheduled-events/{EVENT}")));
    assert!(requests[3].body.is_empty());
    rest.shutdown().await;
}

#[tokio::test]
async fn read_external_location_and_all_four_statuses() {
    for (status, expected) in [
        (1, "SCHEDULED"),
        (2, "ACTIVE"),
        (3, "COMPLETED"),
        (4, "CANCELED"),
    ] {
        let rest = MockRest::start(
            vec![ScriptedResponse::json(200, response(status, false))],
            ScriptedResponse::status(500),
        )
        .await;
        let mirror = MemoryMirror::default();
        let read = executor(&rest)
            .execute_event(
                GUILD,
                &EventCall::Read {
                    event_id: EVENT.to_owned(),
                },
                &mirror,
                stamp,
            )
            .await
            .unwrap();
        assert_eq!(read["status"], expected);
        assert_eq!(read["location"], "The Together We Own server");
        assert_eq!(read.as_object().unwrap().len(), 7);
        assert_eq!(
            mirror.row().unwrap().0.status,
            EventStatus::from_api(status).unwrap()
        );
        rest.shutdown().await;
    }
}

#[tokio::test]
async fn already_cancelled_404_and_other_failures_do_not_retry_or_refresh() {
    // Fresh-key cancellation of an already-cancelled event is a Discord 400,
    // not a fabricated success. Same-key replay is the durable store's job.
    // 404/413/415/422 share the announcement classifier: Discord validates
    // before mutating, so all four are terminal rejections, never fences.
    for (status, code) in [
        (400, "discord_rejected"),
        (403, "discord_rejected"),
        (404, "discord_rejected"),
        (413, "discord_rejected"),
        (415, "discord_rejected"),
        (422, "discord_rejected"),
        (429, "rate_limited"),
        (503, "discord_unavailable"),
    ] {
        let rest = MockRest::start(
            vec![ScriptedResponse::json(
                status,
                json!({"code": 180000, "message": "Cannot update a canceled event"}),
            )],
            ScriptedResponse::status(200),
        )
        .await;
        let mirror = MemoryMirror::default();
        let old = ScheduledEvent {
            id: EVENT.to_owned(),
            name: "old".to_owned(),
            starts_at: "2026-09-01T19:00:00.000Z".to_owned(),
            channel_id: None,
            description: None,
            status: EventStatus::Cancelled,
        };
        mirror.upsert(GUILD, OBSERVED, &old).await.unwrap();
        let error = executor(&rest)
            .execute_event(
                GUILD,
                &EventCall::Cancel {
                    event_id: EVENT.to_owned(),
                },
                &mirror,
                stamp,
            )
            .await
            .unwrap_err();
        assert_eq!(error.action_error().code.as_str(), code);
        assert_eq!(
            error.is_safe_pre_mutation(),
            is_definitive_rejection(status)
        );
        assert_eq!(mirror.row().unwrap().0, old);
        assert_eq!(rest.requests().len(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&rest.requests()[0].body).unwrap(),
            golden()["cancel"]
        );
        rest.shutdown().await;
    }
    // The shared classifier covers every call shape: a mapped event deleted
    // in Discord (PATCH 404) and oversized/unsupported/invalid bodies
    // (413/415/422) are terminal on read and upsert alike, never a fence.
    for status in [404, 413, 415, 422] {
        for call in [
            EventCall::Read {
                event_id: EVENT.to_owned(),
            },
            EventCall::Upsert {
                event_id: Some(EVENT.to_owned()),
                input: input(false),
            },
        ] {
            let rest = MockRest::start(
                vec![ScriptedResponse::status(status)],
                ScriptedResponse::status(500),
            )
            .await;
            let mirror = MemoryMirror::default();
            let error = executor(&rest)
                .execute_event(GUILD, &call, &mirror, stamp)
                .await
                .unwrap_err();
            assert_eq!(error.action_error().code.as_str(), "discord_rejected");
            assert!(error.is_safe_pre_mutation(), "status {status}");
            assert!(mirror.row().is_none());
            assert_eq!(rest.requests().len(), 1);
            rest.shutdown().await;
        }
    }
}

#[tokio::test]
async fn malformed_or_mismatched_success_never_becomes_retry_safe() {
    let mut wrong_id = response(1, false);
    wrong_id["id"] = json!("100000000000000099");
    let mut wrong_guild = response(1, false);
    wrong_guild["guild_id"] = json!("100000000000000099");
    let mut bad_time = response(1, false);
    bad_time["scheduled_start_time"] = json!("not-a-date");
    let mut missing_name = response(1, false);
    missing_name["name"] = Value::Null;
    for body in [
        json!({"id": EVENT}),
        wrong_id,
        wrong_guild,
        bad_time,
        missing_name,
        response(99, false),
    ] {
        let rest = MockRest::start(
            vec![ScriptedResponse::json(200, body)],
            ScriptedResponse::status(500),
        )
        .await;
        let mirror = MemoryMirror::default();
        let error = executor(&rest)
            .execute_event(
                GUILD,
                &EventCall::Upsert {
                    event_id: Some(EVENT.to_owned()),
                    input: input(false),
                },
                &mirror,
                stamp,
            )
            .await
            .unwrap_err();
        assert_eq!(error, EventActionError::InvalidResponse);
        assert!(!error.is_safe_pre_mutation());
        assert!(mirror.row().is_none());
        assert_eq!(rest.requests().len(), 1);
        rest.shutdown().await;
    }
}

#[tokio::test]
async fn missing_create_identity_failed_write_and_bad_local_id_fail_closed() {
    for body in [json!({}), {
        let mut value = response(1, false);
        value["id"] = json!("bad");
        value
    }] {
        let rest = MockRest::start(
            vec![ScriptedResponse::json(201, body)],
            ScriptedResponse::status(500),
        )
        .await;
        let error = executor(&rest)
            .execute_event(
                GUILD,
                &EventCall::Upsert {
                    event_id: None,
                    input: input(false),
                },
                &MemoryMirror::default(),
                stamp,
            )
            .await
            .unwrap_err();
        assert_eq!(error, EventActionError::InvalidResponse);
        assert!(!error.is_safe_pre_mutation());
        assert_eq!(rest.requests().len(), 1);
        rest.shutdown().await;
    }
    let rest = MockRest::start(
        vec![ScriptedResponse::json(200, response(1, false))],
        ScriptedResponse::status(500),
    )
    .await;
    let mirror = MemoryMirror {
        fail: true,
        ..Default::default()
    };
    let error = executor(&rest)
        .execute_event(
            GUILD,
            &EventCall::Upsert {
                event_id: None,
                input: input(false),
            },
            &mirror,
            stamp,
        )
        .await
        .unwrap_err();
    assert_eq!(error, EventActionError::Mirror);
    assert_eq!(error.action_error().code.as_str(), "internal");
    assert!(!error.is_safe_pre_mutation());
    assert_eq!(rest.requests().len(), 1);
    let mut bad_input = input(true);
    bad_input.place = EventPlace::Channel("bad".to_owned());
    let error = executor(&rest)
        .execute_event(
            GUILD,
            &EventCall::Upsert {
                event_id: None,
                input: bad_input,
            },
            &MemoryMirror::default(),
            stamp,
        )
        .await
        .unwrap_err();
    assert!(error.is_safe_pre_mutation());
    assert_eq!(rest.requests().len(), 1);
    rest.shutdown().await;
}

#[tokio::test]
async fn acknowledgement_waits_for_the_mirror_write() {
    #[derive(Default)]
    struct WaitingMirror {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl ScheduledEventMirror for WaitingMirror {
        async fn upsert(&self, _: &str, _: &str, _: &ScheduledEvent) -> Result<(), String> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        }
    }
    let rest = MockRest::start(
        vec![ScriptedResponse::json(201, response(1, false))],
        ScriptedResponse::status(500),
    )
    .await;
    let executor = executor(&rest);
    let mirror = std::sync::Arc::new(WaitingMirror::default());
    let worker_mirror = mirror.clone();
    let worker = tokio::spawn(async move {
        executor
            .execute_event(
                GUILD,
                &EventCall::Upsert {
                    event_id: None,
                    input: input(false),
                },
                worker_mirror.as_ref(),
                stamp,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), mirror.entered.notified())
        .await
        .unwrap();
    assert!(!worker.is_finished());
    mirror.release.notify_one();
    assert_eq!(
        worker.await.unwrap().unwrap(),
        json!({"outcome": "created", "event_id": EVENT})
    );
    assert_eq!(rest.requests().len(), 1);
    rest.shutdown().await;
}

#[cfg(feature = "db")]
mod database {
    use super::*;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{ConnectOptions, PgPool, Row};
    use std::str::FromStr;

    // Same fail-closed effective-options guard as internal_action_store.rs.
    fn test_options(url: &str) -> Result<PgConnectOptions, &'static str> {
        if url.contains(['?', '#']) {
            return Err("no test URL overrides");
        }
        if !(url.starts_with("postgres://agent_test:@")
            || url.starts_with("postgresql://agent_test:@"))
        {
            return Err("explicit agent_test empty password required");
        }
        let options = PgConnectOptions::from_str(url).map_err(|_| "invalid test URL")?;
        if options.get_host() != "agent-testdb"
            || options.get_port() != 5432
            || options.get_socket().is_some()
            || options.get_username() != "agent_test"
            || options.get_options().is_some()
            || options.get_database() != Some("agent_test")
            || options
                .to_url_lossy()
                .password()
                .is_some_and(|password| !password.is_empty())
        {
            return Err("test-container target only");
        }
        Ok(options.password(""))
    }

    #[test]
    fn database_guard_rejects_overrides_and_non_test_targets() {
        assert!(test_options("postgres://agent_test:@agent-testdb:5432/agent_test").is_ok());
        for url in [
            "postgres://agent_test:@production:5432/agent_test",
            "postgres://agent_test:@staging:5432/agent_test",
            "postgres://agent_test@agent-testdb:5432/agent_test",
            "postgres://agent_test:secret@agent-testdb:5432/agent_test",
            "postgres://agent_test:@agent-testdb:5433/agent_test",
            "postgres://agent_test:@agent-testdb:5432/two_bot_test",
            "postgres://agent_test:@agent-testdb:5432/agent_test?host=production",
            "postgres://agent_test:@agent-testdb:5432/agent_test#fragment",
            "postgres://agent_test:@agent-testdb:5432/agent_test?options=-csearch_path=public",
        ] {
            assert!(test_options(url).is_err(), "{url}");
        }
    }

    struct TestDb {
        admin: PgPool,
        pool: PgPool,
        schema: String,
    }

    impl TestDb {
        async fn new() -> Self {
            let url = std::env::var("TWO_TEST_DATABASE_URL")
                .expect("explicit DB test requires TWO_TEST_DATABASE_URL (agent-testdb only)");
            let options = test_options(&url).expect("refusing non-test-container target");
            let admin = PgPoolOptions::new()
                .max_connections(1)
                .connect_with(options.clone())
                .await
                .unwrap();
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let schema = format!("events10862_{}_{stamp}", std::process::id());
            sqlx::query(ddl("CREATE SCHEMA", &schema))
                .execute(&admin)
                .await
                .unwrap();
            let search_path = schema.clone();
            let pool = PgPoolOptions::new()
                .max_connections(2)
                .after_connect(move |connection, _| {
                    let search_path = search_path.clone();
                    Box::pin(async move {
                        sqlx::query("SELECT set_config('search_path', $1, false)")
                            .bind(search_path)
                            .execute(connection)
                            .await?;
                        Ok(())
                    })
                })
                .connect_with(options)
                .await
                .unwrap();
            sqlx::raw_sql(include_str!(
                "../../cutover/migrations/0300_website_contract.sql"
            ))
            .execute(&pool)
            .await
            .unwrap();
            Self {
                admin,
                pool,
                schema,
            }
        }
        async fn assert_row(
            &self,
            status: &str,
            channel: Option<&str>,
            description: Option<&str>,
            observed: &str,
        ) {
            let row =
                sqlx::query("SELECT * FROM scheduled_events WHERE guild_id = $1 AND event_id = $2")
                    .bind(GUILD)
                    .bind(EVENT)
                    .fetch_one(&self.pool)
                    .await
                    .unwrap();
            assert_eq!(row.get::<String, _>("name"), "Launch Night");
            assert_eq!(
                row.get::<String, _>("starts_at"),
                "2026-09-01T19:00:00.000Z"
            );
            assert_eq!(row.get::<String, _>("status"), status);
            assert_eq!(
                row.get::<Option<String>, _>("channel_id").as_deref(),
                channel
            );
            assert_eq!(
                row.get::<Option<String>, _>("description").as_deref(),
                description
            );
            assert_eq!(row.get::<String, _>("updated_at"), observed);
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM scheduled_events")
                    .fetch_one(&self.pool)
                    .await
                    .unwrap(),
                3
            );
        }
        async fn cleanup(self) {
            self.pool.close().await;
            sqlx::query(ddl("DROP SCHEMA", &self.schema))
                .execute(&self.admin)
                .await
                .unwrap();
            self.admin.close().await;
        }
    }

    fn ddl(command: &str, schema: &str) -> sqlx::AssertSqlSafe<String> {
        assert!(schema.starts_with("events10862_") && schema.len() <= 63);
        assert!(schema
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'));
        let suffix = if command == "DROP SCHEMA" {
            " CASCADE"
        } else {
            ""
        };
        sqlx::AssertSqlSafe(format!("{command} {schema}{suffix}"))
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb; CI explicitly runs this test"]
    async fn actions_persist_rows_and_preserve_other_events_and_guilds() {
        let db = TestDb::new().await;
        sqlx::query("INSERT INTO scheduled_events (guild_id, event_id, name, starts_at, status, updated_at) VALUES ($1, 'other', 'sentinel', $3, 'scheduled', $3), ('other-guild', $2, 'sentinel', $3, 'scheduled', $3)").bind(GUILD).bind(EVENT).bind(OBSERVED).execute(&db.pool).await.unwrap();
        let rest = MockRest::start(
            vec![
                ScriptedResponse::json(201, response(1, false)),
                ScriptedResponse::json(200, response(1, true)),
                ScriptedResponse::json(200, response(1, false)),
                ScriptedResponse::json(200, response(4, false)),
                ScriptedResponse::json(200, response(4, false)),
                ScriptedResponse::status(404),
                ScriptedResponse::status(400),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        let executor = executor(&rest);
        executor
            .execute_event(
                GUILD,
                &EventCall::Upsert {
                    event_id: None,
                    input: input(false),
                },
                &db.pool,
                stamp,
            )
            .await
            .unwrap();
        db.assert_row("scheduled", None, None, OBSERVED).await;
        executor
            .execute_event(
                GUILD,
                &EventCall::Upsert {
                    event_id: Some(EVENT.to_owned()),
                    input: input(true),
                },
                &db.pool,
                stamp,
            )
            .await
            .unwrap();
        db.assert_row("scheduled", Some(CHANNEL), Some("Meet in voice"), OBSERVED)
            .await;
        executor
            .execute_event(
                GUILD,
                &EventCall::Upsert {
                    event_id: Some(EVENT.to_owned()),
                    input: input(false),
                },
                &db.pool,
                stamp,
            )
            .await
            .unwrap();
        db.assert_row("scheduled", None, None, OBSERVED).await;
        executor
            .execute_event(
                GUILD,
                &EventCall::Cancel {
                    event_id: EVENT.to_owned(),
                },
                &db.pool,
                stamp,
            )
            .await
            .unwrap();
        db.assert_row("cancelled", None, None, OBSERVED).await;
        let refreshed = "2026-09-01T18:01:00.000Z";
        executor
            .execute_event(
                GUILD,
                &EventCall::Read {
                    event_id: EVENT.to_owned(),
                },
                &db.pool,
                || refreshed.to_owned(),
            )
            .await
            .unwrap();
        db.assert_row("cancelled", None, None, refreshed).await;
        for call in [
            EventCall::Read {
                event_id: EVENT.to_owned(),
            },
            EventCall::Cancel {
                event_id: EVENT.to_owned(),
            },
        ] {
            assert!(executor
                .execute_event(GUILD, &call, &db.pool, stamp)
                .await
                .is_err());
            db.assert_row("cancelled", None, None, refreshed).await;
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM scheduled_events WHERE name = 'sentinel' AND updated_at = $1"
            )
            .bind(OBSERVED)
            .fetch_one(&db.pool)
            .await
            .unwrap(),
            2
        );
        rest.shutdown().await;
        db.cleanup().await;
    }
}
