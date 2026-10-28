//! Review-guard acceptance for the ticket lifecycle observability slice.
//!
//! PROTECTED acceptance handoff for the review-fix round. The paired build
//! consumes this file's head and must not edit it; if a pin looks wrong, hand
//! the correction back to the acceptance author instead of changing the test.
//!
//! Contract pinned here (behavioral, real boundaries: disposable agent-testdb
//! schemas, the shared MockRest double, and the global tracing capture):
//!
//! - `on_ticket_interaction` skips the decorating `warn_ticket_context` lookup
//!   when the failure is itself a SQL storage failure, even when `by_channel`
//!   would still succeed. The `ticket action incomplete` warn then carries
//!   `action` alone (plus `error_class`), never `ticket_id`/`status`.
//! - The same warn lookup is time-boxed to two seconds: expiry degrades to no
//!   context, so a slow store cannot delay the user-visible error reply. The
//!   warn again carries `action` alone.
//! - `recovery_batch` logs `ticket recovery deferred` without `action` when
//!   selection itself failed (transcript-probe failure) or the item timed out,
//!   retaining `ticket_id` and `status` plus `error_class`.
//!
//! The sibling `ticket_lifecycle_observability_acceptance_tests` module stays
//! intact: this file never edits it and uses distinct ticket ids, channels
//! and schema prefixes so parallel suites cannot collide.
//!
//! Database tests use ONLY agent-testdb disposable schemas and are ignored
//! without the test service, like the existing ticket suites. Log attribution
//! uses the shared global capture: action-only warns cannot be filtered by
//! ticket id, so those tests assert the fixture drove a real failure (direct
//! `execute` error class, durable state, MockRest defer+edit) and that no
//! warn in the window cites their ticket id. Deleting the guard makes the
//! warn cite the id and turns the test red.

use super::*;
use crate::{
    command_runtime::CommandRuntime,
    discord_test_common::{MockRest, ScriptedResponse},
};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

// ---------------------------------------------------------------------------
// Fixtures (disposable agent-testdb schemas + shared MockRest double).
// ---------------------------------------------------------------------------

static NEXT_GUARD_SCHEMA: AtomicU64 = AtomicU64::new(0);

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
            "ticket_guard_{}_{}",
            std::process::id(),
            NEXT_GUARD_SCHEMA.fetch_add(1, Ordering::Relaxed)
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
    let runtime = Arc::new(
        TicketRuntime::new(pool, executor, config(), std::collections::HashMap::new()).unwrap(),
    );
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
    }))
    .expect("wire-format ticket interaction")
}

/// Claim that passes staff authorization (roles/permissions) but fails
/// `execute_in_lane` with `InvalidEvidence` before any store access: the
/// member carries no user, so no actor can be extracted. The channel stays
/// present so the warn lookup still attempts its `by_channel` query.
fn button_without_user(action: TicketAction, guild: &str, roles: Vec<&str>) -> Interaction {
    let custom_id = match action {
        TicketAction::Open => TICKET_OPEN_ID,
        TicketAction::Claim => TICKET_CLAIM_ID,
        TicketAction::Close => TICKET_CLOSE_ID,
    };
    serde_json::from_value(json!({
        "application_id":"400", "authorizing_integration_owners":{"0":guild},
        "id":"801", "token":"ticket-test-interaction-token", "type":3, "version":1,
        "guild_id":guild, "channel":{"id":"600","type":0,"name":"ticket-member","permissions":"0"},
        "data":{"custom_id":custom_id,"component_type":2},
        "member":{
            "roles":roles, "permissions":"0", "deaf":false,"mute":false,"flags":0
        }
    }))
    .expect("wire-format ticket interaction without user")
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

async fn active_as(runtime: &TicketRuntime, id: &str, opener: &str, channel: &str) -> Ticket {
    match runtime.store.reserve(id, opener, 0, 0).await.unwrap() {
        OpenResult::Created(_) => {}
        _ => panic!("expected reservation"),
    }
    runtime.store.record_channel(id, channel).await.unwrap();
    runtime.store.activate(id, channel).await.unwrap()
}

fn capture() -> &'static crate::tracing_capture::Capture {
    crate::tracing_capture::global()
}

/// Quote-agnostic field assertion: the capture renders Display fields bare
/// and Debug-quoted strings with quotes, and the contract pins the field
/// names and values, not the sigil the implementation chose.
fn assert_log_field(line: &str, field: &str, value: &str) {
    assert!(
        line.contains(&format!("{field}=")) && line.contains(value),
        "log line misses {field}={value}: {line:?}"
    );
}

fn stale_created_at() -> i64 {
    now_millis_for_test() - INTERRUPTED_AFTER_MS - 60_000
}

// ---------------------------------------------------------------------------
// Storage-failure skip: no second pool acquisition to decorate the warn.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn storage_failure_skips_warn_lookup_even_when_by_channel_succeeds() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
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
    active_as(&tickets, "lc-guard-skip-1", "500", "600").await;
    assert!(tickets.store.by_channel("600").await.unwrap().is_some());

    // Fail every UPDATE for this ticket while leaving SELECTs healthy: the
    // claim write raises a SQL Database error, `by_channel` still resolves.
    sqlx::raw_sql(
        r#"
        CREATE OR REPLACE FUNCTION fail_guard_claim() RETURNS trigger AS $$
        BEGIN RAISE EXCEPTION 'injected guard write failure' USING ERRCODE='raise_exception'; END;
        $$ LANGUAGE plpgsql;
        CREATE TRIGGER fail_guard_claim_trigger BEFORE UPDATE ON tickets
        FOR EACH ROW WHEN (OLD.id = 'lc-guard-skip-1') EXECUTE FUNCTION fail_guard_claim();
        "#,
    )
    .execute(&db.pool)
    .await
    .unwrap();

    // The fixture drives a real storage failure through `execute`.
    let claim = button(TicketAction::Claim, "100", "501", vec!["300"], 0);
    let error = tickets
        .execute(&claim, TicketAction::Claim)
        .await
        .expect_err("claim write must fail with the injected trigger");
    assert!(
        error.is_storage_failure(),
        "fixture must fail as a storage failure, got class {:?}",
        error.class()
    );
    // The decorating lookup would succeed if attempted: the ticket still
    // resolves by channel and the failed write left no claim behind.
    let still = tickets.store.by_channel("600").await.unwrap();
    assert_eq!(
        still.as_ref().map(|ticket| ticket.id.as_str()),
        Some("lc-guard-skip-1"),
        "by_channel must still resolve for the skip pin"
    );
    assert!(tickets
        .store
        .get("lc-guard-skip-1")
        .await
        .unwrap()
        .unwrap()
        .claimed_by
        .is_none());

    // Through the interaction path: defer, failed execute, action-only warn,
    // error reply edit.
    let start = capture().text().len();
    commands
        .on_interaction(&button(TicketAction::Claim, "100", "501", vec!["300"], 0))
        .await;
    assert_eq!(
        mock.requests().len(),
        2,
        "interaction must complete defer+edit through the warn path"
    );
    assert!(tickets
        .store
        .get("lc-guard-skip-1")
        .await
        .unwrap()
        .unwrap()
        .claimed_by
        .is_none());

    // The skip means our warn carries no ticket id; sibling suites use
    // different ids, so none in the window may cite ours. Deleting the
    // `is_storage_failure` guard makes the warn resolve and cite the ticket,
    // turning this red.
    let logs = capture().text().split_off(start);
    for line in logs
        .lines()
        .filter(|line| line.contains("ticket action incomplete"))
    {
        assert!(
            !line.contains("lc-guard-skip-1"),
            "storage-failure warn must not cite the ticket: {line:?}"
        );
    }
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Two-second bound: expiry degrades to no context, reply still delivered.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn warn_lookup_timeout_expiry_degrades_to_action_only() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
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
    active_as(&tickets, "lc-guard-timeout-1", "500", "600").await;

    // The interaction fails before any store access (no member user), so the
    // failure is non-storage and the warn lookup runs. The ticket exists, so
    // an unblocked lookup would resolve and cite it.
    let probe = button_without_user(TicketAction::Claim, "100", vec!["300"]);
    let error = tickets
        .execute(&probe, TicketAction::Claim)
        .await
        .expect_err("userless claim must fail without touching the store");
    assert!(
        !error.is_storage_failure(),
        "timeout pin needs a non-storage failure, got class {:?}",
        error.class()
    );
    assert_eq!(
        tickets
            .store
            .by_channel("600")
            .await
            .unwrap()
            .as_ref()
            .map(|ticket| ticket.id.as_str()),
        Some("lc-guard-timeout-1"),
        "unblocked lookup must resolve for the timeout pin"
    );

    // Exhaust the five-connection pool so the warn lookup blocks. A releaser
    // drops the holds after five seconds: with the two-second bound the
    // interaction already finished action-only; without it (mutant) the
    // lookup unblocks, cites the ticket, and turns the test red.
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(db.pool.acquire().await.unwrap());
    }
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        drop(held);
    });

    let start = capture().text().len();
    commands
        .on_interaction(&button_without_user(
            TicketAction::Claim,
            "100",
            vec!["300"],
        ))
        .await;
    releaser.abort();
    assert_eq!(
        mock.requests().len(),
        2,
        "interaction must complete defer+edit despite the blocked lookup"
    );

    let logs = capture().text().split_off(start);
    for line in logs
        .lines()
        .filter(|line| line.contains("ticket action incomplete"))
    {
        assert!(
            !line.contains("lc-guard-timeout-1"),
            "expired warn lookup must not cite the ticket: {line:?}"
        );
    }
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Recovery deferred without action: probe failure keeps ticket and status.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn recovery_transcript_probe_failure_warns_without_action() {
    let db = TestDb::new().await;
    let mut panel_channel = channel();
    panel_channel["id"] = json!("700");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, panel_channel),
            ScriptedResponse::json(200, json!([])),
            ScriptedResponse::json(200, json!({"id":"801"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    match runtime
        .store
        .reserve("lc-guard-probe-1", "500", stale_created_at(), 0)
        .await
        .unwrap()
    {
        OpenResult::Created(_) => {}
        other => panic!("expected stale reservation, got {other:?}"),
    }
    // The transcript probe is the first store access in recovery: dropping
    // its table fails selection itself, before any action is chosen.
    sqlx::raw_sql("DROP TABLE ticket_transcripts")
        .execute(&db.pool)
        .await
        .unwrap();

    let start = capture().text().len();
    assert!(
        runtime.recover().await.is_err(),
        "probe failure must defer the recovery pass"
    );

    let logs = capture().text().split_off(start);
    let matching: Vec<&str> = logs
        .lines()
        .filter(|line| {
            line.contains("ticket recovery deferred") && line.contains("lc-guard-probe-1")
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one deferred warn for the probe-failed row: {logs:?}"
    );
    assert!(
        !matching[0].contains("action"),
        "probe-failure warn must carry no action: {:?}",
        matching[0]
    );
    assert_log_field(matching[0], "ticket_id", "lc-guard-probe-1");
    assert_log_field(matching[0], "status", TicketStatus::Creating.as_str());
    assert!(
        matching[0].contains("error_class"),
        "deferred warn dropped its error class: {:?}",
        matching[0]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn recovery_batch_none_arm_retains_ticket_and_status_without_action() {
    // Fast offline pin for the same `None` arm: a transcript-probe-shaped
    // outcome (error, no attempted action) warns with ticket and status but
    // no action field. Deleting the arm (always logging an action) turns
    // this red without needing a database.
    let cursor = TaskMutex::new(None);
    let ticket = Ticket {
        id: "lc-guard-batch-1".into(),
        guild_id: "100".into(),
        channel_id: Some("600".into()),
        opener_id: "500".into(),
        claimed_by: None,
        status: TicketStatus::Creating,
        created_at: 0,
        closing_started_at: None,
        closed_at: None,
    };
    let start = capture().text().len();
    let failed = recovery_batch(&cursor, vec![ticket], |_| async {
        (
            Err(Failure::Store(
                two_bot_cutover::tickets::StoreError::NotFound,
            )),
            None,
        )
    })
    .await;
    assert!(failed.is_some(), "error outcome must mark the batch failed");
    let logs = capture().text().split_off(start);
    let matching: Vec<&str> = logs
        .lines()
        .filter(|line| {
            line.contains("ticket recovery deferred") && line.contains("lc-guard-batch-1")
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one deferred warn for the None-arm row: {logs:?}"
    );
    assert!(
        !matching[0].contains("action"),
        "None-arm warn must carry no action: {:?}",
        matching[0]
    );
    assert_log_field(matching[0], "ticket_id", "lc-guard-batch-1");
    assert_log_field(matching[0], "status", TicketStatus::Creating.as_str());
    assert!(
        matching[0].contains("error_class"),
        "deferred warn dropped its error class: {:?}",
        matching[0]
    );
}
