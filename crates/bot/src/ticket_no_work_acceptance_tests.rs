//! Protected acceptance suite: ticket actions that never reserved work must
//! not promise recovery.
//!
//! Two failure paths end before any store reservation exists: waiting for
//! the shared lane can time out before the store is touched, and a
//! claim/close in a channel with no tracked ticket finds nothing to act on.
//! Recovery planning only acts on stored lifecycle states, so neither path
//! creates unfinished work for recovery to retry. The member-visible reply
//! must say so truthfully, while a failure after a real interrupted
//! operation (which does leave a stored state) keeps the existing recovery
//! guidance. These tests pin that distinction at the member-visible surface
//! (`execute` -> reply) and prove the pre-reservation paths write nothing.
//!
//! Uses ONLY agent-testdb and the shared Discord double. Ignored locally
//! without the test service; explicitly enabled in tickets CI.
use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
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
            "ticket_nowork_{}_{}",
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

    async fn row_counts(&self) -> (i64, i64) {
        let tickets: i64 = sqlx::query_scalar("SELECT count(*) FROM tickets")
            .fetch_one(&self.pool)
            .await
            .unwrap();
        let transcripts: i64 = sqlx::query_scalar("SELECT count(*) FROM ticket_transcripts")
            .fetch_one(&self.pool)
            .await
            .unwrap();
        (tickets, transcripts)
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

/// Runtime whose store is never reached: the pool stays lazy, so any test
/// using it fails loudly if the path under test touches the store first.
fn lazy_runtime() -> Arc<TicketRuntime> {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    // A dummy loopback origin: the executor refuses construction without a
    // proxy, and any real Discord call from these paths fails loudly here.
    let executor = ActionExecutor::with_proxy(
        "ticket-test-token".into(),
        Some("http://127.0.0.1:1".into()),
    )
    .unwrap();
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

async fn active(runtime: &TicketRuntime, id: &str) -> Ticket {
    match runtime.store.reserve(id, "500", 0, 0).await.unwrap() {
        OpenResult::Created(_) => {}
        _ => panic!("expected reservation"),
    }
    runtime.store.record_channel(id, "600").await.unwrap();
    runtime.store.activate(id, "600").await.unwrap()
}

/// The exact text a member sees, mirroring the shared router: success text
/// verbatim, failures through the failure-to-reply mapping.
fn member_reply(result: Result<String>) -> String {
    match result {
        Ok(reply) => reply,
        Err(error) => error.reply(),
    }
}

/// A no-work reply must not claim (or even name) a recovery that will never
/// run: recovery planning acts on stored lifecycle states only, and these
/// paths stored nothing. The interrupted-operation control below requires
/// the opposite, pinning the member-visible distinction.
fn assert_no_recovery_promise(reply: &str) {
    let lower = reply.to_lowercase();
    assert!(
        !lower.contains("recover"),
        "reply must not name recovery for work that was never reserved: {reply:?}"
    );
    assert!(
        !lower.contains("unfinished"),
        "reply must not promise to finish work that does not exist: {reply:?}"
    );
    assert!(
        !lower.contains("will retry") && !lower.contains("will be retried"),
        "reply must not promise an automatic retry: {reply:?}"
    );
}

fn assert_no_storage_details(reply: &str) {
    let lower = reply.to_lowercase();
    for detail in ["storeerror", "sqlx", "database", "postgres", "notfound"] {
        assert!(
            !lower.contains(detail),
            "reply must not expose storage detail {detail:?}: {reply:?}"
        );
    }
}

/// Lane admission, fast local form: hold the lane, let paused time drive the
/// admission timeout, and pin the reply. The lazy pool proves the store is
/// not reached before the lane; the database form below proves zero writes.
#[tokio::test(start_paused = true)]
async fn lane_admission_timeout_reply_promises_no_recovery() {
    let runtime = lazy_runtime();
    let _held_lane = runtime.lane.lock().await;
    let reply = member_reply(
        runtime
            .execute(
                &button(TicketAction::Claim, "100", "501", vec!["300"], 0),
                TicketAction::Claim,
            )
            .await,
    );
    assert_no_recovery_promise(&reply);
    assert_no_storage_details(&reply);
    assert!(
        reply.to_lowercase().contains("try again"),
        "lane timeout must stay a truthful retry-later refusal: {reply:?}"
    );
}

/// Domain refusals keep their existing member-visible text exactly: the
/// no-work distinction must not swallow or reword domain errors.
#[tokio::test]
async fn domain_refusals_keep_their_existing_member_visible_text() {
    let runtime = lazy_runtime();
    let wrong_guild = member_reply(
        runtime
            .execute(
                &button(TicketAction::Open, "999", "500", vec![], 0),
                TicketAction::Open,
            )
            .await,
    );
    assert_eq!(wrong_guild, TicketError::WrongGuild.to_string());
    let staff_only = member_reply(
        runtime
            .execute(
                &button(TicketAction::Claim, "100", "500", vec![], 0),
                TicketAction::Claim,
            )
            .await,
    );
    assert_eq!(staff_only, TicketError::StaffOnly.to_string());
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn untracked_channel_claim_and_close_refuse_without_recovery_promise_or_writes() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(db.pool.clone(), &mock);
    assert_eq!(db.row_counts().await, (0, 0));
    for action in [TicketAction::Claim, TicketAction::Close] {
        let reply = member_reply(
            runtime
                .execute(&button(action, "100", "501", vec!["300"], 0), action)
                .await,
        );
        let lower = reply.to_lowercase();
        assert!(
            [
                "no ticket",
                "not a ticket",
                "no open ticket",
                "no active ticket",
                "isn't a ticket",
                "is not a ticket",
            ]
            .iter()
            .any(|specific| lower.contains(specific)),
            "untracked channel needs a specific no-ticket refusal, got: {reply:?}"
        );
        assert_no_recovery_promise(&reply);
        assert_no_storage_details(&reply);
    }
    assert_eq!(
        db.row_counts().await,
        (0, 0),
        "pre-reservation refusals must not reserve or mutate anything"
    );
    assert!(
        mock.requests().is_empty(),
        "pre-reservation refusals must not call Discord"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Lane admission, database form: the lane is held for the whole attempt, so
/// admission times out deterministically before any store work.
#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn lane_admission_timeout_refuses_without_recovery_promise_or_writes() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(db.pool.clone(), &mock);
    assert_eq!(db.row_counts().await, (0, 0));
    let held_lane = runtime.lane.lock().await;
    let reply = member_reply(
        runtime
            .execute(
                &button(TicketAction::Claim, "100", "501", vec!["300"], 0),
                TicketAction::Claim,
            )
            .await,
    );
    drop(held_lane);
    assert_no_recovery_promise(&reply);
    assert_no_storage_details(&reply);
    assert!(
        reply.to_lowercase().contains("try again"),
        "lane timeout must stay a truthful retry-later refusal: {reply:?}"
    );
    assert_eq!(
        db.row_counts().await,
        (0, 0),
        "a lane timeout happens before any reservation and must write nothing"
    );
    assert!(
        mock.requests().is_empty(),
        "a lane timeout happens before any Discord work"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Control: a close interrupted after `begin_close` leaves a stored
/// `closing` lifecycle state that recovery planning really does act on, so
/// the existing recovery guidance stays accurate and must be preserved.
#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn interrupted_close_keeps_recovery_guidance_and_stored_state() {
    let db = TestDb::new().await;
    // Channel fetch and the write-lock succeed; the history capture then
    // fails, interrupting the close after the operation was reserved.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    active(&runtime, "interrupted").await;
    let reply = member_reply(
        runtime
            .execute(
                &button(TicketAction::Close, "100", "501", vec!["300"], 0),
                TicketAction::Close,
            )
            .await,
    );
    let lower = reply.to_lowercase();
    assert!(
        lower.contains("recover"),
        "interrupted operations keep the existing recovery guidance: {reply:?}"
    );
    assert!(
        lower.contains("try again"),
        "interrupted operations keep the existing retry guidance: {reply:?}"
    );
    assert_no_storage_details(&reply);
    let ticket = runtime.store.get("interrupted").await.unwrap().unwrap();
    assert_eq!(
        ticket.status,
        TicketStatus::Closing,
        "the stored closing state is the unfinished work recovery acts on"
    );
    assert!(!runtime
        .store
        .transcript_exists("interrupted")
        .await
        .unwrap());
    assert!(
        mock.requests().iter().all(|r| r.method != "DELETE"),
        "an interrupted close must not delete the channel"
    );
    mock.shutdown().await;
    db.close().await;
}
