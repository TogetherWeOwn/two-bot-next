//! Lifecycle observability acceptance: every ticket retire, abandon, purge and
//! successful claim/close emits one structured log and increments one
//! bounded-label counter.
//!
//! PROTECTED acceptance handoff. The paired build consumes this file's head
//! and must not edit it; if a pin looks wrong, hand the correction back to
//! the acceptance author instead of changing the test.
//!
//! Contract pinned here (behavioral, real boundaries: disposable agent-testdb
//! schemas, the shared MockRest double, the process metrics registry, the
//! global tracing capture, and the docs tree):
//!
//! - Counter `two_bot_ticket_lifecycle_total{event,outcome}`. Events are
//!   exactly `retire | abandon | purge | claim | close`; outcomes are
//!   `retired | abandoned | already_gone | deleted | claimed | closed` plus,
//!   for storage failures, the shared `db_cause` vocabulary. Labels never
//!   carry member text, channel names, ticket ids or error strings.
//! - One `ticket_lifecycle` log per event with `event`, `ticket_id`,
//!   `prior_status` and `cause` fields (`deleted` instead of `ticket_id` for
//!   the pass-level purge line). Causes come from the fixed vocabularies
//!   below; string fields render bare (`ticket_id=...`, not quoted).
//! - `abandon_creating` returning false is counted as `already_gone`, never
//!   silently dropped. Cooldown decisions are unchanged (pinned controls).
//! - The purge pass reports its deleted count (`deleted` in the log and the
//!   counter increment) and preserves the `db_cause` class on storage
//!   failure instead of collapsing everything to `database`.
//! - The `ticket recovery deferred` and `ticket action incomplete` warns keep
//!   `error_class` and gain `action`, `ticket_id` and `status`.
//! - The series, labels and causes are listed in one small docs table next to
//!   the ticket runtime doc, linked from both that doc and the observability
//!   runbook.
//!
//! Boundary with the sibling member-truthfulness slice: this file never pins
//! claim/close reply text, only the counter, the log and the durable state.
//!
//! Database tests use ONLY agent-testdb disposable schemas and are ignored
//! without the test service, like the existing ticket suites. Counter
//! assertions use `>=` because the registry is process-global and sibling
//! suites drive the same runtime paths concurrently; exactness is pinned
//! where attribution is exact (per-ticket-id log lines, `deleted` counts,
//! durable store state). Log attribution uses the shared global capture:
//! each test reads only the text appended after its own start offset.

use super::*;
use crate::{
    command_runtime::CommandRuntime,
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

// ---------------------------------------------------------------------------
// Contract vocabulary. The paired build implements exactly these tokens; a
// needed addition is an acceptance-author correction, not a silent drift.
// ---------------------------------------------------------------------------

/// Counter series for every lifecycle event.
const SERIES: &str = "two_bot_ticket_lifecycle_total";
/// Counter `event` label: the complete vocabulary, in card order.
const EVENTS: &[&str] = &["retire", "abandon", "purge", "claim", "close"];
/// Counter `outcome` label for successful lifecycle events.
const OUTCOMES: &[&str] = &[
    "retired",
    "abandoned",
    "already_gone",
    "deleted",
    "claimed",
    "closed",
];
/// Storage-failure outcomes reuse the shared `db_cause` vocabulary
/// (`two_bot_core::error_chain::db_cause`); SQLSTATE codes, messages and
/// severities never become labels.
const DB_CAUSES: &[&str] = &[
    "unique_violation",
    "foreign_key_violation",
    "not_null_violation",
    "check_violation",
    "exclusion_violation",
    "database",
    "pool_timeout",
    "pool_closed",
    "worker_crashed",
    "connection",
    "row_not_found",
    "query",
];
/// Structured-log message marker for every lifecycle event.
const LOG_LIFECYCLE: &str = "ticket_lifecycle";
/// Log `cause` vocabulary per event.
const RETIRE_CAUSES: &[&str] = &["missing_channel"];
const ABANDON_CAUSES: &[&str] = &["create_rejected", "orphan_not_found", "already_gone"];
const PURGE_CAUSES: &[&str] = &["retention_expired"];
const CLAIM_CAUSES: &[&str] = &["staff_claim"];
const CLOSE_CAUSES: &[&str] = &["staff_close"];
/// `action` vocabulary for the `ticket recovery deferred` warn: one token per
/// fallible recovery action.
const RECOVERY_ACTIONS: &[&str] = &[
    "reattach_open_controls",
    "find_creating_channel",
    "delete_interrupted_create",
    "retry_cleanup",
    "recover_saved_close",
    "restore_opener_then_reopen",
];
/// `action` vocabulary for the `ticket action incomplete` warn.
const TICKET_ACTIONS: &[&str] = &["open", "claim", "close"];
/// Docs table for the series, labels and causes, next to `docs/tickets.md`.
const DOCS_TABLE: &str = "ticket-lifecycle-observability.md";

// ---------------------------------------------------------------------------
// Fixtures (disposable agent-testdb schemas + shared MockRest double).
// ---------------------------------------------------------------------------

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
            "ticket_lifecycle_{}_{}",
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

fn history(id: u64) -> Value {
    json!({"id":id.to_string(),"timestamp":two_bot_core::funnel::format_iso_millis(id as i64),
        "author":{"id":"500","username":"member","discriminator":"0"},
        "content":format!("message-{id}"),"attachments":[{"url":"https://example.test/attachment"}]})
}

async fn active_as(runtime: &TicketRuntime, id: &str, opener: &str, channel: &str) -> Ticket {
    match runtime.store.reserve(id, opener, 0, 0).await.unwrap() {
        OpenResult::Created(_) => {}
        _ => panic!("expected reservation"),
    }
    runtime.store.record_channel(id, channel).await.unwrap();
    runtime.store.activate(id, channel).await.unwrap()
}

fn sample(text: &str, series: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

fn capture() -> &'static crate::tracing_capture::Capture {
    crate::tracing_capture::global()
}

/// Every rendered line of the lifecycle series carries exactly
/// `{event,outcome}` with both values inside the contract vocabularies: a
/// ticket id, member text, channel name or error string can never become a
/// label, and an unknown event or outcome fails loudly instead of rendering.
fn assert_series_shape(text: &str) {
    let mut seen = 0;
    for line in text.lines().filter(|line| line.starts_with(SERIES)) {
        seen += 1;
        let rest = line.strip_prefix(SERIES).unwrap();
        assert!(
            rest.starts_with('{'),
            "lifecycle series carries labels: {line:?}"
        );
        let (labels, value) = rest.split_once("} ").unwrap_or(("", ""));
        value
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("lifecycle sample is not a counter: {line:?}"));
        let labels = labels.strip_prefix('{').unwrap();
        let mut event = None;
        let mut outcome = None;
        for pair in labels.split(',') {
            let (key, quoted) = pair.split_once('=').unwrap_or(("", ""));
            let unquoted = quoted.strip_prefix('"').and_then(|v| v.strip_suffix('"'));
            match key {
                "event" => event = unquoted,
                "outcome" => outcome = unquoted,
                other => panic!("unexpected lifecycle label {other:?} in {line:?}"),
            }
        }
        let (event, outcome) = (
            event.unwrap_or_else(|| panic!("lifecycle line without event: {line:?}")),
            outcome.unwrap_or_else(|| panic!("lifecycle line without outcome: {line:?}")),
        );
        assert!(
            EVENTS.contains(&event),
            "lifecycle event outside the fixed vocabulary: {line:?}"
        );
        assert!(
            OUTCOMES.contains(&outcome) || DB_CAUSES.contains(&outcome),
            "lifecycle outcome outside the fixed vocabulary: {line:?}"
        );
    }
    assert!(seen > 0, "lifecycle series {SERIES} is not rendered");
}

/// Rendered lifecycle lines must not leak the member text, channel names or
/// ticket ids the triggering fixtures carried.
fn assert_series_has_no_leak(text: &str, forbidden: &[&str]) {
    for line in text.lines().filter(|line| line.starts_with(SERIES)) {
        for leak in forbidden {
            assert!(
                !line.contains(leak),
                "lifecycle label leaked {leak:?}: {line:?}"
            );
        }
    }
}

/// Exactly one lifecycle log line for `ticket_id` in the window: one event,
/// one log. Parallel suites use different ticket ids, so the attribution is
/// exact.
fn single_lifecycle_line<'a>(logs: &'a str, ticket_id: &str) -> &'a str {
    let lines: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains(LOG_LIFECYCLE) && line.contains(ticket_id))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "exactly one lifecycle log for {ticket_id:?}: {logs:?}"
    );
    lines[0]
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

/// Visible channel-marker posts (mirrors the member-truthfulness slice):
/// a committed claim posts exactly one `POST .../channels/600/messages`.
fn marker_posts(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|request| {
            request.method == "POST" && request.path.ends_with("/channels/600/messages")
        })
        .collect()
}

fn stale_created_at() -> i64 {
    now_millis_for_test() - INTERRUPTED_AFTER_MS - 60_000
}

// ---------------------------------------------------------------------------
// Retire.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn retire_missing_open_counts_and_logs_once_with_cause() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![ScriptedResponse::json(404, json!({"code":10003}))],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    active_as(&runtime, "lc-retire-1", "500", "600").await;
    let ticket = runtime.store.get("lc-retire-1").await.unwrap().unwrap();

    let start = capture().text().len();
    let outcome = runtime.recover_ticket(&ticket).await;
    assert!(outcome.is_ok(), "recover_ticket should succeed");

    // The retire itself still lands (proves the fixture drove a real retire).
    assert_eq!(
        runtime
            .store
            .get("lc-retire-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        TicketStatus::Closed
    );
    let text = two_bot_core::metrics::global().render(None);
    let series = format!("{SERIES}{{event=\"retire\",outcome=\"retired\"}}");
    assert!(
        sample(&text, &series).is_some_and(|n| n >= 1),
        "missing counter {series}"
    );
    assert_series_shape(&text);
    assert_series_has_no_leak(&text, &["lc-retire-1", "500", "600", "ticket-member"]);

    let logs = capture().text().split_off(start);
    let line = single_lifecycle_line(&logs, "lc-retire-1");
    assert_log_field(line, "event", "retire");
    assert_log_field(line, "ticket_id", "lc-retire-1");
    assert_log_field(line, "prior_status", TicketStatus::Open.as_str());
    assert_log_field(line, "cause", RETIRE_CAUSES[0]);
    assert!(
        !line.contains("ticket-member"),
        "lifecycle log leaked channel name: {line:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Abandon.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn abandon_stale_creating_counts_and_logs_once_with_cause() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!([]))],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    match runtime
        .store
        .reserve("lc-abandon-1", "500", stale_created_at(), 0)
        .await
        .unwrap()
    {
        OpenResult::Created(_) => {}
        other => panic!("expected stale reservation, got {other:?}"),
    }
    let ticket = runtime.store.get("lc-abandon-1").await.unwrap().unwrap();

    let start = capture().text().len();
    let outcome = runtime.recover_ticket(&ticket).await;
    assert!(outcome.is_ok(), "recover_ticket should succeed");

    // The reservation is gone (proves the fixture drove a real abandon).
    assert!(runtime.store.get("lc-abandon-1").await.unwrap().is_none());
    let text = two_bot_core::metrics::global().render(None);
    let series = format!("{SERIES}{{event=\"abandon\",outcome=\"abandoned\"}}");
    assert!(
        sample(&text, &series).is_some_and(|n| n >= 1),
        "missing counter {series}"
    );
    assert_series_shape(&text);
    assert_series_has_no_leak(&text, &["lc-abandon-1", "500", "600"]);

    let logs = capture().text().split_off(start);
    let line = single_lifecycle_line(&logs, "lc-abandon-1");
    assert_log_field(line, "event", "abandon");
    assert_log_field(line, "ticket_id", "lc-abandon-1");
    assert_log_field(line, "prior_status", TicketStatus::Creating.as_str());
    assert_log_field(line, "cause", "orphan_not_found");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn abandon_after_safe_create_rejection_counts_with_cause() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"200","guild_id":"100","type":4})),
            ScriptedResponse::json(403, json!({"code":50013})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    let open = button(TicketAction::Open, "100", "500", vec![], 0);

    let start = capture().text().len();
    assert!(runtime.execute(&open, TicketAction::Open).await.is_err());

    // The rejected reservation is released (existing behavior).
    assert!(runtime.store.recoverable().await.unwrap().is_empty());
    let text = two_bot_core::metrics::global().render(None);
    let series = format!("{SERIES}{{event=\"abandon\",outcome=\"abandoned\"}}");
    assert!(
        sample(&text, &series).is_some_and(|n| n >= 1),
        "missing counter {series}"
    );
    assert_series_shape(&text);

    // The Open path mints a random reservation id, so sibling suites driving
    // the same rejection cannot be told apart: presence, not exact count.
    let logs = capture().text().split_off(start);
    let matching: Vec<&str> = logs
        .lines()
        .filter(|line| {
            line.contains(LOG_LIFECYCLE)
                && line.contains("event")
                && line.contains("abandon")
                && line.contains("create_rejected")
        })
        .collect();
    assert!(
        !matching.is_empty(),
        "missing abandon/create_rejected lifecycle log: {logs:?}"
    );
    assert!(
        matching[0].contains("ticket_id="),
        "abandon log misses its ticket id: {:?}",
        matching[0]
    );
    assert!(
        matching[0].contains(TicketStatus::Creating.as_str()),
        "abandon log misses the creating prior status: {:?}",
        matching[0]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn abandon_creating_false_counts_already_gone_and_logs() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!([]))],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    // A stale in-memory reservation with no durable row: the recovery search
    // finds no orphan and `abandon_creating` deletes zero rows.
    let ticket = Ticket {
        id: "lc-gone-1".into(),
        guild_id: "100".into(),
        channel_id: None,
        opener_id: "500".into(),
        claimed_by: None,
        status: TicketStatus::Creating,
        created_at: stale_created_at(),
        closing_started_at: None,
        closed_at: None,
    };

    let start = capture().text().len();
    let outcome = runtime.recover_ticket(&ticket).await;
    assert!(outcome.is_ok(), "recover_ticket should succeed");

    // Zero rows deleted, and no row invented (the false is handled, not
    // dropped and not mistaken for an abandon).
    assert!(runtime.store.get("lc-gone-1").await.unwrap().is_none());
    let text = two_bot_core::metrics::global().render(None);
    let series = format!("{SERIES}{{event=\"abandon\",outcome=\"already_gone\"}}");
    assert!(
        sample(&text, &series).is_some_and(|n| n >= 1),
        "missing counter {series}"
    );
    assert_series_shape(&text);
    assert_series_has_no_leak(&text, &["lc-gone-1"]);

    let logs = capture().text().split_off(start);
    let line = single_lifecycle_line(&logs, "lc-gone-1");
    assert_log_field(line, "event", "abandon");
    assert_log_field(line, "ticket_id", "lc-gone-1");
    assert_log_field(line, "prior_status", TicketStatus::Creating.as_str());
    assert_log_field(line, "cause", "already_gone");
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Successful claim and close. Reply text belongs to the sibling
// member-truthfulness slice and is never pinned here.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn claim_success_counts_and_logs_once() {
    let db = TestDb::new().await;
    // A committed claim posts one visible channel marker (merged main
    // behavior): script its 200 + id receipt.
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!({"id":"900"}))],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    active_as(&runtime, "lc-claim-1", "500", "600").await;
    let claim = button(TicketAction::Claim, "100", "501", vec!["300"], 0);

    let start = capture().text().len();
    assert!(runtime.execute(&claim, TicketAction::Claim).await.is_ok());

    let ticket = runtime.store.get("lc-claim-1").await.unwrap().unwrap();
    assert_eq!(ticket.claimed_by.as_deref(), Some("501"));
    // Exactly one marker POST naming the claimant; no other Discord I/O.
    let requests = mock.requests();
    let markers = marker_posts(&requests);
    assert_eq!(
        markers.len(),
        1,
        "committed claim posts one channel marker: {requests:?}"
    );
    let body: Value = serde_json::from_slice(&markers[0].body).unwrap();
    let content = body["content"].as_str().expect("visible marker content");
    assert!(
        content.contains("<@501>"),
        "marker must name the claimant: {content}"
    );
    assert!(
        content.to_lowercase().contains("claim"),
        "marker must describe the claim: {content}"
    );
    assert_eq!(
        requests.len(),
        1,
        "claim performs no other Discord I/O: {requests:?}"
    );
    let text = two_bot_core::metrics::global().render(None);
    let series = format!("{SERIES}{{event=\"claim\",outcome=\"claimed\"}}");
    assert!(
        sample(&text, &series).is_some_and(|n| n >= 1),
        "missing counter {series}"
    );
    assert_series_shape(&text);
    assert_series_has_no_leak(
        &text,
        &[
            "lc-claim-1",
            "500",
            "501",
            "600",
            "Member Name",
            "ticket-member",
        ],
    );

    let logs = capture().text().split_off(start);
    let line = single_lifecycle_line(&logs, "lc-claim-1");
    assert_log_field(line, "event", "claim");
    assert_log_field(line, "ticket_id", "lc-claim-1");
    assert_log_field(line, "prior_status", TicketStatus::Open.as_str());
    assert_log_field(line, "cause", CLAIM_CAUSES[0]);
    for leaked in ["Member Name", "ticket-member"] {
        assert!(
            !line.contains(leaked),
            "lifecycle log leaked {leaked:?}: {line:?}"
        );
    }
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn close_success_counts_and_logs_once() {
    let db = TestDb::new().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!([history(1)])),
            ScriptedResponse::json(200, channel()),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock);
    active_as(&runtime, "lc-close-1", "500", "600").await;
    let close = button(
        TicketAction::Close,
        "100",
        "501",
        vec![],
        Permissions::MANAGE_CHANNELS.bits(),
    );

    let start = capture().text().len();
    assert!(runtime.execute(&close, TicketAction::Close).await.is_ok());

    // The close itself still lands with its transcript (existing behavior).
    let ticket = runtime.store.get("lc-close-1").await.unwrap().unwrap();
    assert_eq!(ticket.status, TicketStatus::Closed);
    assert!(runtime.store.transcript_exists("lc-close-1").await.unwrap());
    let text = two_bot_core::metrics::global().render(None);
    let series = format!("{SERIES}{{event=\"close\",outcome=\"closed\"}}");
    assert!(
        sample(&text, &series).is_some_and(|n| n >= 1),
        "missing counter {series}"
    );
    assert_series_shape(&text);
    assert_series_has_no_leak(
        &text,
        &[
            "lc-close-1",
            "500",
            "501",
            "600",
            "Member Name",
            "ticket-member",
            "message-1",
            "example.test",
        ],
    );

    let logs = capture().text().split_off(start);
    let line = single_lifecycle_line(&logs, "lc-close-1");
    assert_log_field(line, "event", "close");
    assert_log_field(line, "ticket_id", "lc-close-1");
    assert_log_field(line, "prior_status", TicketStatus::Open.as_str());
    assert_log_field(line, "cause", CLOSE_CAUSES[0]);
    for leaked in ["Member Name", "ticket-member", "message-1", "example.test"] {
        assert!(
            !line.contains(leaked),
            "lifecycle log leaked {leaked:?}: {line:?}"
        );
    }
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Purge.
// ---------------------------------------------------------------------------

async fn finished_with_transcript(runtime: &TicketRuntime, id: &str, opener: &str, channel: &str) {
    active_as(runtime, id, opener, channel).await;
    runtime.store.begin_close(id, 1).await.unwrap();
    runtime
        .store
        .save_transcript(id, 1, 2, format_transcript(vec![]))
        .await
        .unwrap();
    runtime.store.finish_cleanup(id, 3).await.unwrap();
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn purge_reports_deleted_count_in_counter_and_log() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(db.pool.clone(), &mock);
    finished_with_transcript(&runtime, "lc-purge-1", "501", "600").await;
    finished_with_transcript(&runtime, "lc-purge-2", "502", "601").await;

    let start = capture().text().len();
    assert!(runtime.purge().await.is_ok());

    // Both expired bodies are gone (proves the purge really deleted two).
    assert!(!runtime.store.transcript_exists("lc-purge-1").await.unwrap());
    assert!(!runtime.store.transcript_exists("lc-purge-2").await.unwrap());
    let text = two_bot_core::metrics::global().render(None);
    let series = format!("{SERIES}{{event=\"purge\",outcome=\"deleted\"}}");
    assert!(
        sample(&text, &series).is_some_and(|n| n >= 2),
        "missing counter {series} incremented by the deleted count"
    );
    assert_series_shape(&text);

    // The pass-level line carries the count; no sibling suite purges exactly
    // two bodies, so the count attributes the line to this pass.
    let logs = capture().text().split_off(start);
    let matching: Vec<&str> = logs
        .lines()
        .filter(|line| {
            line.contains(LOG_LIFECYCLE) && line.contains("event") && line.contains("purge")
        })
        .collect();
    assert!(
        matching.iter().any(|line| line.contains("deleted=2")),
        "purge log misses its deleted count: {logs:?}"
    );
    assert!(
        matching
            .iter()
            .any(|line| line.contains("cause") && line.contains(PURGE_CAUSES[0])),
        "purge log misses its cause: {logs:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn purge_db_failure_preserves_cause_instead_of_collapsing() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(db.pool.clone(), &mock);
    // A closed pool fails every purge query with `PoolClosed`, deterministically.
    db.pool.close().await;

    let start = capture().text().len();
    assert!(runtime.purge().await.is_err());

    let text = two_bot_core::metrics::global().render(None);
    let series = format!("{SERIES}{{event=\"purge\",outcome=\"pool_closed\"}}");
    assert!(
        sample(&text, &series).is_some_and(|n| n >= 1),
        "missing counter {series}: the DB failure class must survive, not collapse"
    );
    assert_series_shape(&text);

    // Only this test closes its pool, so the cause attributes the line.
    let logs = capture().text().split_off(start);
    let matching: Vec<&str> = logs
        .lines()
        .filter(|line| {
            line.contains(LOG_LIFECYCLE)
                && line.contains("event")
                && line.contains("purge")
                && line.contains("pool_closed")
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one purge log preserving pool_closed: {logs:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Warns keep `error_class` and gain `action`, `ticket_id`, `status`.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn recovery_deferred_warn_carries_action_ticket_and_status() {
    let db = TestDb::new().await;
    let mut panel_channel = channel();
    panel_channel["id"] = json!("700");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(500),
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
        .reserve("lc-deferred-1", "500", stale_created_at(), 0)
        .await
        .unwrap()
    {
        OpenResult::Created(_) => {}
        other => panic!("expected stale reservation, got {other:?}"),
    }

    let start = capture().text().len();
    // The guild search fails, so the row's recovery defers (existing shape).
    assert!(runtime.recover().await.is_err());

    let logs = capture().text().split_off(start);
    let matching: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("ticket recovery deferred") && line.contains("lc-deferred-1"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one deferred warn for the failed row: {logs:?}"
    );
    assert_log_field(matching[0], "action", "find_creating_channel");
    assert_log_field(matching[0], "ticket_id", "lc-deferred-1");
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
#[ignore = "requires agent-testdb or CI service container"]
async fn ticket_action_incomplete_warn_carries_action_ticket_and_status() {
    let db = TestDb::new().await;
    // Wire order per interaction is defer, then (inside execute) the claim-1
    // channel marker, then the deferred-reply edit. The marker consumes one
    // script slot, so without it the second defer receives the wrong receipt
    // and fails before execute runs. Edits keep their `{}` bodies (tolerated
    // edit failures, as before); only the defers must succeed.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id":"900"})),
            ScriptedResponse::json(200, json!({})),
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
    active_as(&tickets, "lc-warn-1", "500", "600").await;
    commands
        .on_interaction(&button(TicketAction::Claim, "100", "501", vec!["300"], 0))
        .await;
    assert_eq!(
        tickets
            .store
            .get("lc-warn-1")
            .await
            .unwrap()
            .unwrap()
            .claimed_by
            .as_deref(),
        Some("501")
    );

    // A second staffer's claim fails against the already-claimed ticket.
    let start = capture().text().len();
    commands
        .on_interaction(&button(TicketAction::Claim, "100", "502", vec!["300"], 0))
        .await;

    // First winner still holds the claim (existing behavior).
    assert_eq!(
        tickets
            .store
            .get("lc-warn-1")
            .await
            .unwrap()
            .unwrap()
            .claimed_by
            .as_deref(),
        Some("501")
    );
    let logs = capture().text().split_off(start);
    let matching: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("ticket action incomplete") && line.contains("lc-warn-1"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one incomplete warn for the failed claim: {logs:?}"
    );
    assert_log_field(matching[0], "action", "claim");
    assert_log_field(matching[0], "ticket_id", "lc-warn-1");
    assert_log_field(matching[0], "status", TicketStatus::Open.as_str());
    assert!(
        matching[0].contains("error_class"),
        "incomplete warn dropped its error class: {:?}",
        matching[0]
    );
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Unchanged-behavior pins. These pass before and after the build: the
// observability slice must not move cooldown or reservation semantics.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn cooldown_decisions_are_unchanged() {
    let db = TestDb::new().await;
    let store = TicketStore::new(db.pool.clone(), "100".into()).unwrap();
    let now = now_millis_for_test();

    // An active reservation refuses a second open as existing, not cooldown.
    assert!(matches!(
        store
            .reserve("lc-cd-1", "lc-op-cd", now, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    assert!(matches!(
        store
            .reserve("lc-cd-2", "lc-op-cd", now + 1, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Refused(OpenDecision::Existing { .. })
    ));

    // With no active row, a recent close still cools down, then frees.
    sqlx::query("UPDATE tickets SET status='closed' WHERE id='lc-cd-1'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        store
            .reserve("lc-cd-3", "lc-op-cd", now + 2, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Refused(OpenDecision::Cooldown { .. })
    ));
    assert!(matches!(
        store
            .reserve(
                "lc-cd-4",
                "lc-op-cd",
                now + COOLDOWN_SECONDS as i64 * 1000,
                COOLDOWN_SECONDS
            )
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn abandon_creating_frees_reservation_and_cooldown_unchanged() {
    let db = TestDb::new().await;
    let store = TicketStore::new(db.pool.clone(), "100".into()).unwrap();
    let now = now_millis_for_test();

    assert!(matches!(
        store
            .reserve("lc-cd-5", "lc-op-cd2", now, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    // Deleting the reservation row deletes its cooldown record with it.
    assert!(store.abandon_creating("lc-cd-5").await.unwrap());
    assert!(matches!(
        store
            .reserve("lc-cd-6", "lc-op-cd2", now + 1, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    // A second abandon deletes zero rows and reports false.
    assert!(!store.abandon_creating("lc-cd-5").await.unwrap());
    db.close().await;
}

// ---------------------------------------------------------------------------
// Offline pins: docs table and vocabulary hygiene. No DB, no Discord.
// ---------------------------------------------------------------------------

fn docs_file(name: &str) -> String {
    let path = format!("{}/../../docs/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("docs table is part of the slice: missing {path}"))
}

#[test]
fn lifecycle_docs_table_lists_series_labels_and_causes() {
    let table = docs_file(DOCS_TABLE);
    for required in [SERIES, "event", "outcome"] {
        assert!(table.contains(required), "docs table misses {required:?}");
    }
    for token in EVENTS
        .iter()
        .chain(OUTCOMES.iter())
        .chain(DB_CAUSES.iter())
        .chain(RETIRE_CAUSES.iter())
        .chain(ABANDON_CAUSES.iter())
        .chain(PURGE_CAUSES.iter())
        .chain(CLAIM_CAUSES.iter())
        .chain(CLOSE_CAUSES.iter())
        .chain(RECOVERY_ACTIONS.iter())
        .chain(TICKET_ACTIONS.iter())
    {
        assert!(table.contains(*token), "docs table misses token {token:?}");
    }
    for field in [
        "ticket_id",
        "prior_status",
        "cause",
        "deleted",
        "action",
        "status",
    ] {
        assert!(table.contains(field), "docs table misses field {field:?}");
    }
    // Discoverable from the ticket runtime doc and the observability runbook.
    for (doc, name) in [
        (docs_file("tickets.md"), "tickets.md"),
        (
            docs_file("runbook-observability.md"),
            "runbook-observability.md",
        ),
    ] {
        assert!(
            doc.contains(DOCS_TABLE),
            "{name} does not link the lifecycle docs table"
        );
    }
}

#[test]
fn cause_vocabularies_are_bounded_distinct_tokens() {
    fn is_token(value: &str) -> bool {
        (1..=32).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }
    // Each vocabulary is internally distinct; the same token may appear in
    // two roles (`already_gone` is both an outcome and a cause).
    for vocabulary in [
        EVENTS,
        OUTCOMES,
        DB_CAUSES,
        RETIRE_CAUSES,
        ABANDON_CAUSES,
        PURGE_CAUSES,
        CLAIM_CAUSES,
        CLOSE_CAUSES,
        RECOVERY_ACTIONS,
        TICKET_ACTIONS,
    ] {
        let mut seen = std::collections::BTreeSet::new();
        for token in vocabulary {
            assert!(
                is_token(token),
                "vocabulary token is not bounded: {token:?}"
            );
            assert!(seen.insert(*token), "duplicate vocabulary token {token:?}");
        }
    }
    assert!(
        SERIES
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "series name is not a bounded token: {SERIES:?}"
    );
}
