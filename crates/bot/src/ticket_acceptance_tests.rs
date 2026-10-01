//! Combined acceptance uses ONLY agent-testdb and the shared Discord double.
//! Ignored locally without the test service; explicitly enabled in tickets CI.
use super::*;
use crate::{
    command_runtime::CommandRuntime,
    mock_rest::{MockRest, ScriptedResponse},
};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    pool: PgPool,
    admin: PgPool,
    schema: String,
}

impl TestDb {
    async fn new() -> Self {
        match std::env::var("TICKET_TEST_DB_HOST").as_deref() {
            Ok("agent-testdb") | Err(_) => {}
            _ => {
                panic!("ticket acceptance permits only agent-testdb; never substitute credentials")
            }
        }
        let options = PgConnectOptions::new()
            .host("agent-testdb")
            .port(5432)
            .username("agent_test")
            .password("")
            .database("agent_test");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("connect to authorized test container");
        let schema = format!(
            "ticket_runtime_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::QueryBuilder::new("CREATE SCHEMA ")
            .push(&schema)
            .build()
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options.options([("search_path", schema.clone())]))
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../cutover/migrations/0210_tickets.sql"))
            .execute(&pool)
            .await
            .unwrap();
        Self {
            pool,
            admin,
            schema,
        }
    }

    async fn close(self) {
        self.pool.close().await;
        sqlx::QueryBuilder::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

fn config() -> TicketConfig {
    TicketConfig {
        guild_id: "100".into(),
        category_id: "200".into(),
        panel_channel_id: "700".into(),
        staff_role_id: "300".into(),
        cooldown_seconds: COOLDOWN_SECONDS,
    }
}

fn runtime(pool: PgPool, mock: &MockRest) -> Arc<TicketRuntime> {
    let executor =
        ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let runtime = Arc::new(TicketRuntime::new(pool, executor, config()).unwrap());
    runtime.set_bot_id(400);
    runtime
}

fn button(
    action: TicketAction,
    guild: &str,
    actor: &str,
    roles: Vec<&str>,
    permissions: u64,
) -> Interaction {
    let custom_id = match action {
        TicketAction::Open => TICKET_OPEN_ID,
        TicketAction::Claim => TICKET_CLAIM_ID,
        TicketAction::Close => TICKET_CLOSE_ID,
    };
    serde_json::from_value(json!({
        "application_id":"400", "authorizing_integration_owners":{"0":guild},
        "id":"800", "token":"ticket-test-interaction-token", "type":3, "version":1,
        "guild_id":guild, "channel":{"id":"600","type":0,"name":"ticket-member","permissions":"0"},
        "data":{"custom_id":custom_id,"component_type":2},
        "member":{
            "roles":roles, "permissions":permissions.to_string(), "deaf":false,"mute":false,"flags":0,
            "user":{"id":actor,"username":"Member Name","discriminator":"0","avatar":null}
        }
    })).expect("wire-format ticket interaction")
}

fn channel() -> Value {
    let common = (Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY)
        .bits();
    json!({"id":"600","guild_id":"100","type":0,"parent_id":"200",
    "permission_overwrites":[
        {"id":"400","type":1,"allow":(common | Permissions::MANAGE_CHANNELS.bits()).to_string(),"deny":"0"},
        {"id":"500","type":1,"allow":common.to_string(),"deny":"0"}
    ]})
}

fn history(id: u64) -> Value {
    json!({"id":id.to_string(),"timestamp":two_bot_core::funnel::format_iso_millis(id as i64),
        "author":{"id":"500","username":"member","discriminator":"0"},
        "content":format!("message-{id}"),"attachments":[{"url":"https://example.test/attachment"}]})
}

async fn active(runtime: &TicketRuntime, id: &str) -> Ticket {
    match runtime.store.reserve(id, "500", 0, 0).await.unwrap() {
        OpenResult::Created(_) => {}
        _ => panic!("expected reservation"),
    }
    runtime.store.record_channel(id, "600").await.unwrap();
    runtime.store.activate(id, "600").await.unwrap()
}

async fn wait_for_request(mock: &MockRest, method: &str, suffix: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if mock
                .requests()
                .iter()
                .any(|r| r.method == method && r.path.ends_with(suffix))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("mock request must arrive");
}

#[tokio::test]
async fn shared_router_refuses_unauthorized_and_foreign_guild_before_database_work() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let tickets = runtime(pool.clone(), &mock);
    let commands =
        CommandRuntime::with_tickets(pool, tickets.executor.clone(), Arc::clone(&tickets));
    for action in [TicketAction::Claim, TicketAction::Close] {
        commands
            .on_interaction(&button(action, "100", "501", vec![], 0))
            .await;
        assert!(tickets
            .authorize(&button(action, "100", "501", vec!["300"], 0), action)
            .is_ok());
        for permissions in [Permissions::MANAGE_CHANNELS, Permissions::ADMINISTRATOR] {
            assert!(tickets
                .authorize(
                    &button(action, "100", "501", vec![], permissions.bits()),
                    action
                )
                .is_ok());
        }
    }
    for action in [TicketAction::Open, TicketAction::Claim, TicketAction::Close] {
        commands
            .on_interaction(&button(
                action,
                "999",
                "501",
                vec!["300"],
                Permissions::ADMINISTRATOR.bits(),
            ))
            .await;
    }
    assert_eq!(
        mock.requests().len(),
        2,
        "foreign-guild router fence is silent"
    );
    for request in mock.requests() {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["type"], 4);
        assert_eq!(body["data"]["flags"], 64);
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn failed_shared_defer_never_mutates_or_creates_a_channel() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let tickets = runtime(pool.clone(), &mock);
    let commands = CommandRuntime::with_tickets(pool, tickets.executor.clone(), tickets);
    commands
        .on_interaction(&button(TicketAction::Open, "100", "500", vec![], 0))
        .await;
    assert!(mock
        .requests()
        .iter()
        .all(|r| r.path.ends_with("/callback")));
    mock.shutdown().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn shared_router_lifecycle_defers_ephemerally_and_edits_without_leaking_transcript() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id":"200","guild_id":"100","type":4})),
            ScriptedResponse::json(201, json!({"id":"600"})),
            ScriptedResponse::json(200, json!({"id":"900"})),
            ScriptedResponse::json(200, json!({})),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({})),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!([history(1)])),
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let tickets = runtime(db.pool.clone(), &mock);
    let commands = CommandRuntime::with_tickets(
        db.pool.clone(),
        tickets.executor.clone(),
        Arc::clone(&tickets),
    );
    commands
        .on_interaction(&button(TicketAction::Open, "100", "500", vec![], 0))
        .await;
    commands
        .on_interaction(&button(TicketAction::Claim, "100", "501", vec!["300"], 0))
        .await;
    commands
        .on_interaction(&button(
            TicketAction::Close,
            "100",
            "501",
            vec![],
            Permissions::MANAGE_CHANNELS.bits(),
        ))
        .await;
    let ticket = tickets.store.by_channel("600").await.unwrap().unwrap();
    assert_eq!(ticket.status, TicketStatus::Closed);
    assert_eq!(ticket.claimed_by.as_deref(), Some("501"));
    assert!(tickets.store.transcript_exists(&ticket.id).await.unwrap());
    let requests = mock.requests();
    assert_eq!(requests.len(), 14);
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .collect();
    assert_eq!(callbacks.len(), 3);
    for request in callbacks {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["type"], 5);
        assert_eq!(body["data"]["flags"], 64);
    }
    let edits: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/messages/@original"))
        .collect();
    assert_eq!(edits.len(), 3);
    assert!(edits
        .iter()
        .all(|r| !String::from_utf8_lossy(&r.body).contains("message-1")));
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn lifecycle_races_and_transcript_commit_precede_discord_delete() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"200","guild_id":"100","type":4})),
            ScriptedResponse::json(201, json!({"id":"600"})),
            ScriptedResponse::json(200, json!({"id":"900"})),
            ScriptedResponse::json(200, json!({"id":"200","guild_id":"100","type":4})),
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
            ScriptedResponse::json(
                200,
                json!((901..=1000).rev().map(history).collect::<Vec<_>>()),
            ),
            ScriptedResponse::json(200, json!([history(900), history(899)])),
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204).delayed(Duration::from_secs(1)),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    let open = button(TicketAction::Open, "100", "500", vec![], 0);
    let (a, b) = tokio::join!(
        runtime.execute(&open, TicketAction::Open),
        runtime.execute(&open, TicketAction::Open)
    );
    assert!(a.is_ok() && b.is_ok());
    let tickets = runtime.store.recoverable().await.unwrap();
    assert_eq!(tickets.len(), 1);
    let ticket = tickets[0].clone();
    let create = mock
        .requests()
        .into_iter()
        .find(|r| r.path.ends_with("/guilds/100/channels"))
        .unwrap();
    let body: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(body["name"], "ticket-member-name");
    assert_eq!(body["parent_id"], "200");
    assert_eq!(body["topic"], format!("two-ticket:{}", ticket.id));
    assert_eq!(body["permission_overwrites"].as_array().unwrap().len(), 4);
    let staff_a = button(TicketAction::Claim, "100", "501", vec!["300"], 0);
    let staff_b = button(TicketAction::Claim, "100", "502", vec!["300"], 0);
    let (a, b) = tokio::join!(
        runtime.execute(&staff_a, TicketAction::Claim),
        runtime.execute(&staff_b, TicketAction::Claim)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let closing_runtime = Arc::clone(&runtime);
    let closes = tokio::spawn(async move {
        tokio::join!(
            closing_runtime.execute(&staff_a, TicketAction::Close),
            closing_runtime.execute(&staff_b, TicketAction::Close)
        )
    });
    wait_for_request(&mock, "DELETE", "/channels/600").await;
    let during_delete = runtime.store.get(&ticket.id).await.unwrap().unwrap();
    assert_eq!(during_delete.status, TicketStatus::CleanupPending);
    let (content, count): (String, i32) =
        sqlx::query_as("SELECT content, message_count FROM ticket_transcripts WHERE ticket_id=$1")
            .bind(&ticket.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(count, 102);
    assert!(content.contains("https://example.test/attachment"));
    assert!(content.find("message-899").unwrap() < content.find("message-1000").unwrap());
    let (a, b) = closes.await.unwrap();
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(
        runtime.store.get(&ticket.id).await.unwrap().unwrap().status,
        TicketStatus::Closed
    );
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "DELETE")
            .count(),
        1
    );
    let foreign = TicketStore::new(db.pool.clone(), "999".into()).unwrap();
    assert!(foreign.by_channel("600").await.unwrap().is_none());
    assert!(foreign.recoverable().await.unwrap().is_empty());
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn partial_history_or_failed_insert_never_deletes_and_unsaved_close_recovers() {
    for failed_insert in [false, true] {
        let db = TestDb::new().await;
        if failed_insert {
            sqlx::raw_sql(
                "ALTER TABLE ticket_transcripts ADD CONSTRAINT reject_capture CHECK (false)",
            )
            .execute(&db.pool)
            .await
            .unwrap();
        }
        let mut script = vec![
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
        ];
        if failed_insert {
            script.push(ScriptedResponse::json(200, json!([history(1)])));
        } else {
            script.push(ScriptedResponse::json(
                200,
                json!((901..=1000).rev().map(history).collect::<Vec<_>>()),
            ));
            script.push(ScriptedResponse::status(500));
        }
        script.extend([
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
        ]);
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let runtime = runtime(db.pool.clone(), &mock);
        let ticket = active(&runtime, "unsaved").await;
        assert!(runtime.close(&ticket).await.is_err());
        assert!(!runtime.store.transcript_exists("unsaved").await.unwrap());
        assert_eq!(
            runtime.store.get("unsaved").await.unwrap().unwrap().status,
            TicketStatus::Closing
        );
        assert!(mock.requests().iter().all(|r| r.method != "DELETE"));
        sqlx::query("UPDATE tickets SET closing_started_at=$1 WHERE id='unsaved'")
            .bind(two_bot_core::funnel::format_iso_millis(
                now_millis_for_test() - INTERRUPTED_AFTER_MS - 1,
            ))
            .execute(&db.pool)
            .await
            .unwrap();
        let stale = runtime.store.get("unsaved").await.unwrap().unwrap();
        assert!(runtime.recover_ticket(&stale).await.is_ok());
        assert_eq!(
            runtime.store.get("unsaved").await.unwrap().unwrap().status,
            TicketStatus::Open
        );
        let put = mock
            .requests()
            .into_iter()
            .rfind(|r| r.method == "PUT")
            .unwrap();
        let body: Value = serde_json::from_slice(&put.body).unwrap();
        let allow: u64 = body["allow"].as_str().unwrap().parse().unwrap();
        assert_ne!(allow & Permissions::SEND_MESSAGES.bits(), 0);
        assert!(runtime
            .store
            .save_transcript(
                "unsaved",
                stale.closing_started_at.unwrap(),
                now_millis_for_test(),
                format_transcript(vec![])
            )
            .await
            .is_err());
        mock.shutdown().await;
        db.close().await;
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn uncertain_create_survives_restart_and_uses_exact_topic_without_duplicate_create() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"200","guild_id":"100","type":4})),
            ScriptedResponse::json(201, json!({"unexpected":"missing channel id"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let first = runtime(db.pool.clone(), &mock);
    assert!(first
        .execute(
            &button(TicketAction::Open, "100", "500", vec![], 0),
            TicketAction::Open
        )
        .await
        .is_err());
    let ticket = first.store.recoverable().await.unwrap().remove(0);
    assert_eq!(ticket.status, TicketStatus::Creating);
    assert!(ticket.channel_id.is_none());
    sqlx::query("UPDATE tickets SET created_at=$1 WHERE id=$2")
        .bind(two_bot_core::funnel::format_iso_millis(
            now_millis_for_test() - INTERRUPTED_AFTER_MS - 1,
        ))
        .bind(&ticket.id)
        .execute(&db.pool)
        .await
        .unwrap();
    mock.shutdown().await;
    drop(first);
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(
                200,
                json!([{"id":"600","type":0,"topic":format!("two-ticket:{}",ticket.id)}]),
            ),
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let restarted = runtime(db.pool.clone(), &mock);
    let stale = restarted.store.get(&ticket.id).await.unwrap().unwrap();
    assert!(restarted.recover_ticket(&stale).await.is_ok());
    assert_eq!(
        restarted
            .store
            .get(&ticket.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TicketStatus::Closed
    );
    assert!(mock.requests().iter().all(|r| r.method != "POST"));
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn safe_create_rejection_releases_reservation_but_failed_controls_cleanup_is_durable() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"200","guild_id":"100","type":4})),
            ScriptedResponse::json(403, json!({"code":50013})),
            ScriptedResponse::json(200, json!({"id":"200","guild_id":"100","type":4})),
            ScriptedResponse::json(201, json!({"id":"600"})),
            ScriptedResponse::status(500),
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::json(403, json!({"code":50013})),
            ScriptedResponse::json(404, json!({"code":10003})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    let open = button(TicketAction::Open, "100", "500", vec![], 0);
    assert!(runtime.execute(&open, TicketAction::Open).await.is_err());
    assert!(runtime.store.recoverable().await.unwrap().is_empty());
    assert!(runtime.execute(&open, TicketAction::Open).await.is_err());
    let ticket = runtime.store.recoverable().await.unwrap().remove(0);
    assert_eq!(ticket.status, TicketStatus::CleanupPending);
    assert_eq!(ticket.channel_id.as_deref(), Some("600"));
    assert!(!runtime.store.transcript_exists(&ticket.id).await.unwrap());
    assert!(runtime.recover_ticket(&ticket).await.is_ok());
    assert_eq!(
        runtime.store.get(&ticket.id).await.unwrap().unwrap().status,
        TicketStatus::Closed
    );
    mock.shutdown().await;
    db.close().await;
}

fn own_controls() -> Value {
    json!({"author":{"id":"400"},"components":[{"type":1,"components":[
        {"type":2,"custom_id":TICKET_CLAIM_ID}, {"type":2,"custom_id":TICKET_CLOSE_ID}
    ]}]})
}

fn own_panel() -> Value {
    json!({"author":{"id":"400"},"components":[{"type":1,"components":[
        {"type":2,"custom_id":TICKET_OPEN_ID}
    ]}]})
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn ready_recovers_controls_and_panel_without_duplicates_and_purges_expired_bodies() {
    let db = TestDb::new().await;
    let mut panel_channel = channel();
    panel_channel["id"] = json!("700");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::json(200, json!([])),
            ScriptedResponse::json(200, json!({"id":"800"})),
            ScriptedResponse::json(200, panel_channel.clone()),
            ScriptedResponse::json(200, json!([])),
            ScriptedResponse::json(200, json!({"id":"801"})),
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::json(200, json!([own_controls()])),
            ScriptedResponse::json(200, panel_channel),
            ScriptedResponse::json(200, json!([own_panel()])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    active(&runtime, "expired").await;
    runtime.store.begin_close("expired", 1).await.unwrap();
    runtime
        .store
        .save_transcript("expired", 1, 2, format_transcript(vec![]))
        .await
        .unwrap();
    runtime.store.finish_cleanup("expired", 3).await.unwrap();
    // Remove the closed channel association to seed a different current channel.
    sqlx::query("UPDATE tickets SET channel_id='601' WHERE id='expired'")
        .execute(&db.pool)
        .await
        .unwrap();
    // The old closed row must not exercise cooldown for this fresh ticket.
    active(&runtime, "open").await;
    let supervisor = runtime.start().unwrap();
    runtime.on_ready(400);
    wait_for_request(&mock, "POST", "/channels/700/messages").await;
    // Wait for each inline maintenance lane to leave before inspecting its result.
    let recovery_guard = runtime.lane.lock().await;
    let purge_guard = runtime.purge_lane.lock().await;
    assert!(!runtime.store.transcript_exists("expired").await.unwrap());
    drop(recovery_guard);
    drop(purge_guard);
    assert!(runtime.recover().await.is_ok());
    assert_eq!(mock.requests().len(), 10);
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        2
    );
    supervisor.shutdown().await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn legacy_saved_close_and_member_erasure_preserve_guild_fences() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![ScriptedResponse::json(404, json!({"code":10003}))],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    active(&runtime, "legacy").await;
    runtime.store.begin_close("legacy", 1).await.unwrap();
    runtime
        .store
        .save_transcript(
            "legacy",
            1,
            now_millis_for_test(),
            format_transcript(vec![]),
        )
        .await
        .unwrap();
    // Emulate a legacy crash between INSERT and the separate cleanup transition.
    sqlx::query("UPDATE tickets SET status='closing', closed_at=NULL WHERE id='legacy'")
        .execute(&db.pool)
        .await
        .unwrap();
    let ticket = runtime.store.get("legacy").await.unwrap().unwrap();
    assert!(runtime.recover_ticket(&ticket).await.is_ok());
    assert_eq!(
        runtime.store.get("legacy").await.unwrap().unwrap().status,
        TicketStatus::Closed
    );
    let foreign = TicketStore::new(db.pool.clone(), "999".into()).unwrap();
    assert_eq!(foreign.erase_member("500").await.unwrap(), 0);
    assert!(runtime.store.transcript_exists("legacy").await.unwrap());
    assert_eq!(runtime.store.erase_member("500").await.unwrap(), 1);
    assert!(!runtime.store.transcript_exists("legacy").await.unwrap());
    assert!(runtime.store.get("legacy").await.unwrap().is_none());
    assert!(mock.requests().iter().all(|r| r.method != "PUT"));
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn saved_close_access_failure_stays_durable_after_inclusive_purge_and_never_reopens() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::json(403, json!({"code":50013})),
            ScriptedResponse::json(404, json!({"code":10003})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    active(&runtime, "saved").await;
    runtime.store.begin_close("saved", 1).await.unwrap();
    runtime
        .store
        .save_transcript("saved", 1, 2, format_transcript(vec![]))
        .await
        .unwrap();
    let pending = runtime.store.get("saved").await.unwrap().unwrap();
    assert!(runtime.recover_ticket(&pending).await.is_err());
    assert_eq!(
        runtime.store.get("saved").await.unwrap().unwrap().status,
        TicketStatus::CleanupPending
    );
    assert!(runtime.store.transcript_exists("saved").await.unwrap());
    assert_eq!(
        runtime
            .store
            .purge_expired(2 + TRANSCRIPT_RETENTION_MS - 1)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        runtime
            .store
            .purge_expired(2 + TRANSCRIPT_RETENTION_MS)
            .await
            .unwrap(),
        1
    );
    assert!(!runtime.store.transcript_exists("saved").await.unwrap());
    let pending = runtime.store.get("saved").await.unwrap().unwrap();
    assert!(runtime.recover_ticket(&pending).await.is_ok());
    assert_eq!(
        runtime.store.get("saved").await.unwrap().unwrap().status,
        TicketStatus::Closed
    );
    assert!(mock.requests().iter().all(|r| r.method != "PUT"));
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn missing_open_channel_requires_typed_absence_and_releases_the_reservation() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(403, json!({"code":50013})),
            ScriptedResponse::status(500),
            ScriptedResponse::json(404, json!({"message":"Unknown Channel 10003"})),
            ScriptedResponse::json(404, json!({"code":"10003"})),
            ScriptedResponse::json(404, json!({"code":10003})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    let ticket = active(&runtime, "missing").await;
    for _ in 0..4 {
        assert!(runtime.recover_ticket(&ticket).await.is_err());
        assert_eq!(
            runtime.store.get("missing").await.unwrap().unwrap().status,
            TicketStatus::Open
        );
        assert!(matches!(
            runtime.store.reserve("blocked", "500", 1, 0).await.unwrap(),
            OpenResult::Refused(OpenDecision::Existing { .. })
        ));
        assert!(!runtime.store.transcript_exists("missing").await.unwrap());
    }
    assert!(runtime.recover_ticket(&ticket).await.is_ok());
    assert_eq!(
        runtime.store.get("missing").await.unwrap().unwrap().status,
        TicketStatus::Closed
    );
    assert!(!runtime.store.transcript_exists("missing").await.unwrap());
    assert!(matches!(
        runtime
            .store
            .reserve("replacement", "500", 2, 0)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    assert_eq!(mock.requests().len(), 5);
    assert!(mock.requests().iter().all(|r| r.method == "GET"));
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn missing_open_retirement_rechecks_guild_channel_state_and_saved_capture() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(db.pool.clone(), &mock);
    active(&runtime, "fenced").await;
    assert!(runtime
        .store
        .retire_missing_open("fenced", "601", 2)
        .await
        .is_err());
    let foreign = TicketStore::new(db.pool.clone(), "999".into()).unwrap();
    assert!(foreign
        .retire_missing_open("fenced", "600", 2)
        .await
        .is_err());
    assert_eq!(
        runtime.store.get("fenced").await.unwrap().unwrap().status,
        TicketStatus::Open
    );
    runtime.store.begin_close("fenced", 3).await.unwrap();
    assert!(runtime
        .store
        .retire_missing_open("fenced", "600", 4)
        .await
        .is_err());
    runtime
        .store
        .save_transcript("fenced", 3, 4, format_transcript(vec![]))
        .await
        .unwrap();
    assert!(runtime
        .store
        .retire_missing_open("fenced", "600", 5)
        .await
        .is_err());
    // A legacy inconsistent open row must not erase or reinterpret saved evidence.
    sqlx::query("UPDATE tickets SET status='open', closing_started_at=NULL, closed_at=NULL WHERE id='fenced'")
        .execute(&db.pool).await.unwrap();
    assert!(runtime
        .store
        .retire_missing_open("fenced", "600", 5)
        .await
        .is_err());
    assert!(runtime.store.transcript_exists("fenced").await.unwrap());
    assert_eq!(
        runtime.store.get("fenced").await.unwrap().unwrap().status,
        TicketStatus::Open
    );
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn cold_resumed_dispatch_recovers_controls_panel_and_purges_without_ready() {
    let db = TestDb::new().await;
    let mut panel = channel();
    panel["id"] = json!("700");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"400","bot":true})),
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::json(200, json!([])),
            ScriptedResponse::json(200, json!({"id":"800"})),
            ScriptedResponse::json(200, panel),
            ScriptedResponse::json(200, json!([])),
            ScriptedResponse::json(200, json!({"id":"801"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let executor =
        ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let tickets =
        Arc::new(TicketRuntime::new(db.pool.clone(), executor.clone(), config()).unwrap());
    active(&tickets, "expired-resume").await;
    tickets
        .store
        .begin_close("expired-resume", 1)
        .await
        .unwrap();
    tickets
        .store
        .save_transcript("expired-resume", 1, 2, format_transcript(vec![]))
        .await
        .unwrap();
    tickets
        .store
        .finish_cleanup("expired-resume", 3)
        .await
        .unwrap();
    sqlx::query("UPDATE tickets SET channel_id='601' WHERE id='expired-resume'")
        .execute(&db.pool)
        .await
        .unwrap();
    active(&tickets, "open-resume").await;
    let commands = CommandRuntime::with_tickets(db.pool.clone(), executor, Arc::clone(&tickets));
    commands.suppress_registry_for_test().await;
    let supervisor = commands.start_tickets().unwrap();
    assert_eq!(tickets.readiness_for_test(), (0, 0));
    commands.dispatch(&twilight_model::gateway::event::Event::Resumed);
    wait_for_request(&mock, "POST", "/channels/700/messages").await;
    let recovery_guard = tickets.lane.lock().await;
    let purge_guard = tickets.purge_lane.lock().await;
    assert_eq!(tickets.readiness_for_test(), (400, 1));
    assert!(!tickets
        .store
        .transcript_exists("expired-resume")
        .await
        .unwrap());
    assert_eq!(
        tickets
            .store
            .get("open-resume")
            .await
            .unwrap()
            .unwrap()
            .status,
        TicketStatus::Open
    );
    assert_eq!(mock.requests().len(), 7);
    assert_eq!(mock.requests()[0].path, "/api/v10/users/@me");
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        2
    );
    drop(recovery_guard);
    drop(purge_guard);
    supervisor.shutdown().await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn cancelled_close_drops_partial_capture_and_recovery_restores_unsaved_ticket() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!([history(1)])).delayed(Duration::from_secs(1)),
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    active(&runtime, "cancelled").await;
    let work_runtime = Arc::clone(&runtime);
    let task = tokio::spawn(async move {
        work_runtime
            .execute(
                &button(TicketAction::Close, "100", "501", vec!["300"], 0),
                TicketAction::Close,
            )
            .await
    });
    wait_for_request(&mock, "GET", "/channels/600/messages?limit=100").await;
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    assert!(runtime.lane.try_lock().is_ok());
    assert_eq!(
        runtime
            .store
            .get("cancelled")
            .await
            .unwrap()
            .unwrap()
            .status,
        TicketStatus::Closing
    );
    assert!(!runtime.store.transcript_exists("cancelled").await.unwrap());
    assert!(mock.requests().iter().all(|r| r.method != "DELETE"));
    sqlx::query("UPDATE tickets SET closing_started_at=$1 WHERE id='cancelled'")
        .bind(two_bot_core::funnel::format_iso_millis(
            now_millis_for_test() - INTERRUPTED_AFTER_MS - 1,
        ))
        .execute(&db.pool)
        .await
        .unwrap();
    let stale = runtime.store.get("cancelled").await.unwrap().unwrap();
    assert!(runtime.recover_ticket(&stale).await.is_ok());
    assert_eq!(
        runtime
            .store
            .get("cancelled")
            .await
            .unwrap()
            .unwrap()
            .status,
        TicketStatus::Open
    );
    assert!(runtime
        .store
        .save_transcript(
            "cancelled",
            stale.closing_started_at.unwrap(),
            now_millis_for_test(),
            format_transcript(vec![])
        )
        .await
        .is_err());
    assert!(mock.requests().iter().all(|r| r.method != "DELETE"));
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn failed_cold_resumed_identity_retries_during_recovery_without_another_gateway_event() {
    let db = TestDb::new().await;
    let mut panel = channel();
    panel["id"] = json!("700");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(500),
            ScriptedResponse::json(200, json!({"id":"400","bot":true})),
            ScriptedResponse::json(200, panel),
            ScriptedResponse::json(200, json!([])),
            ScriptedResponse::json(200, json!({"id":"801"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let executor =
        ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let tickets =
        Arc::new(TicketRuntime::new(db.pool.clone(), executor.clone(), config()).unwrap());
    let commands = CommandRuntime::with_tickets(db.pool.clone(), executor, Arc::clone(&tickets));
    commands.suppress_registry_for_test().await;
    commands.dispatch(&twilight_model::gateway::event::Event::Resumed);
    // Await the actual managed identity task, not merely request arrival.
    let mut tasks = {
        let mut tasks = tickets.tasks.lock().unwrap();
        std::mem::take(&mut *tasks)
    };
    while tasks.join_next().await.is_some() {}
    assert_eq!(tickets.readiness_for_test(), (0, 0));
    assert_eq!(mock.requests().len(), 1);
    // Exercise the same recovery action invoked by the 300-second timer.
    assert!(tickets.recover().await.is_ok());
    assert_eq!(tickets.readiness_for_test(), (400, 1));
    assert_eq!(mock.requests().len(), 5);
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.path == "/api/v10/users/@me")
            .count(),
        2
    );
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        1
    );
    mock.shutdown().await;
    db.close().await;
}
