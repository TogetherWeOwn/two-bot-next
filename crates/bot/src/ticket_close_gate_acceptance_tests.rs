//! Acceptance: a ticket close whose history-access gate fails must be
//! refused BEFORE any mutation — the ticket stays `Open`, the opener keeps
//! write access, and the reply is a clear refusal rather than the generic
//! "recovery will retry" copy. A gate-passing close keeps the exact current
//! path (revoke → transcript → cleanup), and a genuinely interrupted close
//! still recovers by restoring the opener.
//!
//! Uses ONLY agent-testdb and the shared Discord double. Ignored locally
//! without the test service; the tickets CI job runs `ticket_runtime::`
//! tests with `--include-ignored`, so these run there unmodified.
use super::*;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// The generic reply every non-domain failure produces today. A gate refusal
/// must not promise recovery for work that was never started.
const RECOVERY_COPY: &str =
    "Ticket action failed; recovery will retry unfinished work. Try again later.";

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
            "ticket_close_gate_{}_{}",
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

fn runtime(pool: PgPool, mock: &MockRest) -> Arc<TicketRuntime> {
    let executor =
        ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let runtime = Arc::new(
        TicketRuntime::new(
            pool,
            executor,
            TicketConfig {
                guild_id: "100".into(),
                category_id: "200".into(),
                panel_channel_id: "700".into(),
                staff_role_id: "300".into(),
                cooldown_seconds: COOLDOWN_SECONDS,
            },
            std::collections::HashMap::new(),
        )
        .unwrap(),
    );
    runtime.set_bot_id(400);
    runtime
}

/// Route-aware Discord double: the same channel document answers every
/// channel fetch however the close path orders its reads, while every
/// mutation route is recorded so a test can demand zero of them.
async fn discord(channel: Value, history_ok: bool) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let path = request
            .path
            .strip_prefix("/api/v10")
            .unwrap_or(&request.path);
        let path = path.split('?').next().unwrap_or(path);
        match (request.method.as_str(), path) {
            ("GET", "/channels/600") => ScriptedResponse::json(200, channel.clone()),
            ("GET", "/channels/600/messages") => {
                if history_ok {
                    ScriptedResponse::json(200, json!([history(1)]))
                } else {
                    ScriptedResponse::status(500)
                }
            }
            ("PUT", "/channels/600/permissions/500") | ("DELETE", "/channels/600") => {
                ScriptedResponse::status(204)
            }
            _ => ScriptedResponse::status(500),
        }
    })
    .await
}

fn history(id: u64) -> Value {
    json!({"id":id.to_string(),"timestamp":two_bot_core::funnel::format_iso_millis(id as i64),
        "author":{"id":"500","username":"member","discriminator":"0"},
        "content":format!("message-{id}"),"attachments":[{"url":"https://example.test/attachment"}]})
}

fn common_bits() -> u64 {
    (Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::READ_MESSAGE_HISTORY)
        .bits()
}

fn opener_overwrite() -> Value {
    json!({"id":"500","type":1,"allow":common_bits().to_string(),"deny":"0"})
}

fn channel_with(mut overwrites: Vec<Value>) -> Value {
    overwrites.push(opener_overwrite());
    json!({"id":"600","guild_id":"100","type":0,"parent_id":"200",
    "permission_overwrites":overwrites})
}

/// The healthy ticket channel `open()` creates: explicit member overwrites
/// for the bot (id 400) and the opener (id 500).
fn gated_channel() -> Value {
    channel_with(vec![json!({"id":"400","type":1,
        "allow":(common_bits() | Permissions::MANAGE_CHANNELS.bits()).to_string(),"deny":"0"})])
}

/// Channel shapes whose explicit-overwrite gate must refuse, e.g. after a
/// hand edit in Discord: access may exist via category/role permissions,
/// but the bot has no explicit member overwrite proving history access.
fn ungated_channels() -> Vec<(&'static str, Value)> {
    vec![
        (
            "no bot overwrite at all",
            channel_with(vec![json!({"id":"300","type":0,
                "allow":common_bits().to_string(),"deny":"0"})]),
        ),
        (
            "bot access only via a role overwrite",
            channel_with(vec![json!({"id":"400","type":0,
                "allow":common_bits().to_string(),"deny":"0"})]),
        ),
        (
            "bot member overwrite without history access",
            channel_with(vec![json!({"id":"400","type":1,
                "allow":Permissions::VIEW_CHANNEL.bits().to_string(),"deny":"0"})]),
        ),
        (
            "bot member overwrite denying history access",
            channel_with(vec![json!({"id":"400","type":1,
                "allow":common_bits().to_string(),
                "deny":Permissions::READ_MESSAGE_HISTORY.bits().to_string()})]),
        ),
    ]
}

async fn active(runtime: &TicketRuntime, id: &str) -> Ticket {
    match runtime.store.reserve(id, "500", 0, 0).await.unwrap() {
        OpenResult::Created(_) => {}
        _ => panic!("expected reservation"),
    }
    runtime.store.record_channel(id, "600").await.unwrap();
    runtime.store.activate(id, "600").await.unwrap()
}

fn staff_close_button() -> Interaction {
    serde_json::from_value(json!({
        "application_id":"400", "authorizing_integration_owners":{"0":"100"},
        "id":"800", "token":"ticket-test-interaction-token", "type":3, "version":1,
        "guild_id":"100", "channel":{"id":"600","type":0,"name":"ticket-member","permissions":"0"},
        "data":{"custom_id":TICKET_CLOSE_ID,"component_type":2},
        "member":{
            "roles":["300"], "permissions":"0", "deaf":false,"mute":false,"flags":0,
            "user":{"id":"501","username":"Staff Name","discriminator":"0","avatar":null}
        }
    }))
    .expect("wire-format ticket interaction")
}

/// Every recorded request is a read: no permission PUT, no channel DELETE,
/// no message POST reached Discord.
fn assert_reads_only(mock: &MockRest, context: &str) {
    let requests = mock.requests();
    assert!(
        !requests.is_empty(),
        "{context}: the gate reads the channel"
    );
    assert!(
        requests.iter().all(|request| request.method == "GET"),
        "{context}: zero channel-write mutations, got {:?}",
        requests
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn close_gate_refusal_leaves_ticket_open_and_opener_writes_untouched() {
    for (shape, channel) in ungated_channels() {
        let db = TestDb::new().await;
        let mock = discord(channel, true).await;
        let runtime = runtime(db.pool.clone(), &mock);
        let ticket = active(&runtime, "gate-refused").await;

        assert!(
            runtime.close(&ticket).await.is_err(),
            "{shape}: close must be refused"
        );

        let after = runtime
            .store
            .get("gate-refused")
            .await
            .unwrap()
            .expect("ticket row");
        assert_eq!(after.status, TicketStatus::Open, "{shape}");
        assert_eq!(after.closing_started_at, None, "{shape}");
        assert_eq!(after.closed_at, None, "{shape}");
        assert!(
            !runtime
                .store
                .transcript_exists("gate-refused")
                .await
                .unwrap(),
            "{shape}"
        );
        assert_reads_only(&mock, shape);
        mock.shutdown().await;
        db.close().await;
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn close_gate_refusal_reply_is_a_clear_refusal_not_the_recovery_copy() {
    let db = TestDb::new().await;
    let (_, channel) = ungated_channels().into_iter().next().expect("fixture");
    let mock = discord(channel, true).await;
    let runtime = runtime(db.pool.clone(), &mock);
    active(&runtime, "gate-refused-reply").await;

    let error = runtime
        .execute(&staff_close_button(), TicketAction::Close)
        .await
        .expect_err("staff close on an ungated channel is refused");
    let reply = error.reply();
    assert_ne!(reply, RECOVERY_COPY);
    assert!(!reply.trim().is_empty(), "a refusal says something");
    assert!(
        reply.to_lowercase().contains("clos"),
        "the refusal names the refused close, got: {reply}"
    );

    let after = runtime
        .store
        .get("gate-refused-reply")
        .await
        .unwrap()
        .expect("ticket row");
    assert_eq!(after.status, TicketStatus::Open);
    assert_reads_only(&mock, "staff close refusal");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn close_gate_pass_keeps_the_full_close_path() {
    let db = TestDb::new().await;
    let mock = discord(gated_channel(), true).await;
    let runtime = runtime(db.pool.clone(), &mock);
    let ticket = active(&runtime, "gate-passed").await;

    runtime
        .close(&ticket)
        .await
        .ok()
        .expect("gated close succeeds");

    let after = runtime
        .store
        .get("gate-passed")
        .await
        .unwrap()
        .expect("ticket row");
    assert_eq!(after.status, TicketStatus::Closed);
    let (content, count): (String, i32) =
        sqlx::query_as("SELECT content, message_count FROM ticket_transcripts WHERE ticket_id=$1")
            .bind("gate-passed")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    assert!(content.contains("message-1"));

    let requests = mock.requests();
    let index_of = |method: &str, needle: &str| {
        requests
            .iter()
            .position(|request| request.method == method && request.path.contains(needle))
    };
    let revoke = index_of("PUT", "/channels/600/permissions/500").expect("opener writes revoked");
    let delete = index_of("DELETE", "/channels/600").expect("channel deleted in cleanup");
    let fetch = index_of("GET", "/channels/600").expect("channel fetched");
    // The channel is read (and gated) before writes are revoked, and the
    // channel survives until after the transcript commit drives cleanup.
    assert!(fetch < revoke, "channel read precedes revocation");
    assert!(revoke < delete, "revocation precedes cleanup delete");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .count(),
        1
    );
    let body: Value = serde_json::from_slice(&requests[revoke].body).unwrap();
    let allow: u64 = body["allow"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        allow & Permissions::SEND_MESSAGES.bits(),
        0,
        "the revocation removes the opener's SendMessages"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn interrupted_close_after_gate_pass_still_recovers_by_restoring_opener() {
    let db = TestDb::new().await;
    // History capture fails after the gate passed and writes were revoked:
    // this is the genuinely interrupted close recovery exists for.
    let mock = discord(gated_channel(), false).await;
    let runtime = runtime(db.pool.clone(), &mock);
    let ticket = active(&runtime, "interrupted").await;

    assert!(runtime.close(&ticket).await.is_err());
    let wedged = runtime
        .store
        .get("interrupted")
        .await
        .unwrap()
        .expect("ticket row");
    assert_eq!(wedged.status, TicketStatus::Closing);
    assert!(!runtime
        .store
        .transcript_exists("interrupted")
        .await
        .unwrap());
    assert!(mock.requests().iter().all(|r| r.method != "DELETE"));

    sqlx::query("UPDATE tickets SET closing_started_at=$1 WHERE id='interrupted'")
        .bind(two_bot_core::funnel::format_iso_millis(
            now_millis_for_test() - INTERRUPTED_AFTER_MS - 1,
        ))
        .execute(&db.pool)
        .await
        .unwrap();
    let stale = runtime.store.get("interrupted").await.unwrap().unwrap();
    runtime
        .recover_ticket(&stale)
        .await
        .ok()
        .expect("interrupted close recovers");
    assert_eq!(
        runtime
            .store
            .get("interrupted")
            .await
            .unwrap()
            .unwrap()
            .status,
        TicketStatus::Open
    );
    let restore = mock
        .requests()
        .into_iter()
        .rfind(|r| r.method == "PUT")
        .expect("recovery restores the opener's overwrite");
    let body: Value = serde_json::from_slice(&restore.body).unwrap();
    let allow: u64 = body["allow"].as_str().unwrap().parse().unwrap();
    assert_ne!(allow & Permissions::SEND_MESSAGES.bits(), 0);
    mock.shutdown().await;
    db.close().await;
}
