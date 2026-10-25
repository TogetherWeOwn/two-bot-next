//! Protected acceptance: a completed scheduled-message failure must not skip
//! eligible channel reminders. Exercise the real ticker, stores, executor and
//! supervisor; only Discord and failures in the disposable database are faked.
//! The separate test-only module registration is integration scaffolding.
#![cfg(test)]

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::sync::watch;
use tracing::Instrument as _;
use two_bot_core::{
    format_iso_ms, get_scheduled, put_scheduled, FeatureGates, ScheduledMessageRow, ScheduledWrite,
};
use two_bot_discord::{executor::ActionExecutor, ratelimit_guard::RateLimitGuard};
use two_bot_testsupport::TestDatabase;

use super::super::tick;
use crate::{
    activation::fixtures,
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
    jobs::{self, ErrorClass, JobAction},
};

const NOW: u64 = 1_790_000_000_000;
const GUILD: &str = "3333";
const SCHEDULE_CHANNEL: &str = "4444";
const CHANNEL: &str = "1546451670500642111";
const EVENT: &str = "1546451670500642999";
const EVENT2: &str = "1546451670500643001";
const EVENT3: &str = "1546451670500643002";
const USER: &str = "1546451670500642888";
const DECLINED: &str = "1546451670500642777";
const PRIVATE_NAME: &str = "phase-isolation-private-event @everyone";
const PRIVATE_BODY: &str = "phase-isolation-private-scheduled-body";

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded disposable database"
        );
        eprintln!("SKIP phase isolation: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create disposable phase-isolation database"),
    )
}

async fn schedule(pool: &PgPool, id: &str, due: u64) {
    let at = format_iso_ms(NOW - 60_000);
    assert!(put_scheduled(
        pool,
        &ScheduledWrite {
            id: id.to_owned(),
            guild_id: GUILD.to_owned(),
            channel_id: SCHEDULE_CHANNEL.to_owned(),
            body: PRIVATE_BODY.to_owned(),
            next_run_at: format_iso_ms(due),
            interval_seconds: None,
            enabled: true,
            created_by: "5555".to_owned(),
            created_at: at.clone(),
            updated_by: "5555".to_owned(),
            updated_at: at,
        },
    )
    .await
    .expect("seed scheduled message"));
}

async fn event(pool: &PgPool, guild: &str, id: &str, status: &str, channel: Option<&str>) {
    sqlx::query(
        "INSERT INTO scheduled_events
         (guild_id,event_id,name,starts_at,channel_id,status,updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(guild)
    .bind(id)
    .bind(PRIVATE_NAME)
    .bind(format_iso_ms(NOW + 900_000))
    .bind(channel)
    .bind(status)
    .bind(format_iso_ms(NOW))
    .execute(pool)
    .await
    .expect("seed event mirror");
    for (user, status) in [(USER, "going"), (DECLINED, "declined")] {
        sqlx::query(
            "INSERT INTO event_rsvps (guild_id,event_id,user_id,status,responded_at)
             VALUES ($1,$2,$3,$4,$5::text::timestamptz)",
        )
        .bind(guild)
        .bind(id)
        .bind(user)
        .bind(status)
        .bind(format_iso_ms(NOW - 60_000))
        .execute(pool)
        .await
        .expect("seed stored RSVP");
    }
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy_and_guard(
        "synthetic-phase-isolation-token".to_owned(),
        Some(mock.origin()),
        Arc::new(RateLimitGuard::new(Default::default()).expect("isolated pacing guard")),
    )
    .expect("loopback Discord executor")
}

fn posted() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"id": "9001"}))
}

async fn rest(
    schedule_response: ScriptedResponse,
    reminder_response: ScriptedResponse,
) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| match request.method.as_str() {
        "DELETE" if request.path.starts_with("/api/v10/channels/4444/messages/") => {
            ScriptedResponse::status(204)
        }
        "POST" if request.path == "/api/v10/channels/4444/messages" => schedule_response.clone(),
        "POST" if request.path == format!("/api/v10/channels/{CHANNEL}/messages") => {
            reminder_response.clone()
        }
        _ => ScriptedResponse::status(404),
    })
    .await
}

async fn run(pool: &PgPool, rest: &ActionExecutor, now: u64) -> Result<(), ErrorClass> {
    tokio::time::timeout(Duration::from_secs(20), tick(pool, rest, GUILD, || now))
        .await
        .expect("bounded tick must terminate")
}

// Diagnostics attribution: the global capture is shared by every parallel
// test in this binary, so a byte window alone cannot tell this tick's phase
// diagnostics from a sibling's fixture SQL or phase lines. Each exercised
// tick runs inside a span carrying a unique `capture_case`; the capture tags
// lines emitted while that span is entered, and only tagged lines are
// scanned. Sibling diagnostics can neither contaminate nor satisfy the scan.
fn next_capture_case(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "phase-isolation-{label}-{}",
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

async fn run_attributed(
    pool: &PgPool,
    rest: &ActionExecutor,
    now: u64,
    case: &str,
) -> Result<(), ErrorClass> {
    tokio::time::timeout(
        Duration::from_secs(20),
        tick(pool, rest, GUILD, || now).instrument(tracing::info_span!(
            "phase_isolation_tick",
            capture_case = case
        )),
    )
    .await
    .expect("bounded tick must terminate")
}

fn attributed(capture: &crate::tracing_capture::Capture, start: usize, case: &str) -> String {
    let marker = format!(" capture_case={case}");
    capture.text()[start..]
        .lines()
        .filter(|line| line.contains(&format!("{marker} ")) || line.ends_with(&marker))
        .collect::<Vec<_>>()
        .join("\n")
}

// Positive contamination control: a concurrent sibling span (and an
// unspanned event) emitting the exact sensitive values and phase words must
// neither fail this case's content-free scan nor satisfy a case that emitted
// nothing, while this case's own classified lines stay visible.
#[tokio::test]
async fn capture_attribution_ignores_concurrent_sibling_diagnostics() {
    let capture = crate::tracing_capture::global();
    let start = capture.text().len();
    let case = next_capture_case("control");
    let sibling = next_capture_case("sibling");
    let mine = async {
        tracing::warn!(reason = "database", "scheduled message store failed");
        tokio::task::yield_now().await;
        tracing::warn!(reason = "database", "event reminder store failed");
    }
    .instrument(tracing::info_span!(
        "phase_isolation_tick",
        capture_case = case.as_str()
    ));
    let poison = async {
        tracing::warn!(
            "scheduled reminder database {} {} {} private-phase-store-error-sentinel",
            PRIVATE_BODY,
            PRIVATE_NAME,
            USER
        );
        tokio::task::yield_now().await;
    }
    .instrument(tracing::info_span!(
        "phase_isolation_tick",
        capture_case = sibling.as_str()
    ));
    tokio::join!(mine, poison);
    tracing::warn!("scheduled reminder database {}", PRIVATE_BODY);

    let mine = attributed(capture, start, &case);
    for phase in ["scheduled", "reminder"] {
        assert!(
            mine.lines()
                .any(|line| line.contains(phase) && line.contains("database")),
            "this case's classified phase diagnostics stay attributable"
        );
    }
    for sensitive in [
        PRIVATE_BODY,
        PRIVATE_NAME,
        USER,
        "private-phase-store-error-sentinel",
    ] {
        assert!(
            !mine.contains(sensitive),
            "concurrent sibling diagnostics must not contaminate this case"
        );
    }
    assert!(
        attributed(capture, start, &sibling).contains(PRIVATE_BODY),
        "the sibling poison was really captured, under its own case only"
    );
    assert!(
        attributed(capture, start, "phase-isolation-absent-0").is_empty(),
        "sibling diagnostics cannot satisfy a case that emitted nothing"
    );
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct Fact {
    id: String,
    actor_id: Option<String>,
    action: String,
    target_key: Option<String>,
    outcome: String,
    reason: Option<String>,
    created_at: String,
}

async fn facts(pool: &PgPool, id: &str) -> Vec<Fact> {
    sqlx::query_as(
        "SELECT id,actor_id,action,target_key,outcome,reason,created_at::text
         FROM automation_audit_log WHERE guild_id=$1 AND target_key=$2
         AND action IN ('rsvp.reminder_attempt','rsvp.reminder') ORDER BY action,id",
    )
    .bind(GUILD)
    .bind(id)
    .fetch_all(pool)
    .await
    .expect("read committed reminder evidence")
}

fn assert_fenced(rows: &[Fact], id: &str, outcome: Option<&str>) {
    let attempts: Vec<_> = rows
        .iter()
        .filter(|r| r.action == "rsvp.reminder_attempt")
        .collect();
    assert_eq!(attempts.len(), 1, "one committed pre-wire reminder fence");
    let attempt = attempts[0];
    assert_eq!(attempt.id, format!("event:reminder:{GUILD}:{id}"));
    assert_eq!(attempt.actor_id, None);
    assert_eq!(attempt.target_key.as_deref(), Some(id));
    assert_eq!(attempt.outcome, "unknown");
    assert_eq!(attempt.reason.as_deref(), Some("delivery_pending"));
    let results: Vec<_> = rows
        .iter()
        .filter(|r| r.action == "rsvp.reminder")
        .collect();
    if let Some(outcome) = outcome {
        assert_eq!(
            results.len(),
            1,
            "one separate result, never a rewritten fence"
        );
        assert_ne!(results[0].id, attempt.id);
        assert_eq!(results[0].outcome, outcome);
        assert_eq!(results[0].actor_id, None);
        assert_eq!(results[0].target_key.as_deref(), Some(id));
    } else {
        assert!(results.is_empty(), "no invented delivery receipt");
    }
}

fn posts(requests: &[RestRequest], channel: &str) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r.method == "POST" && r.path == format!("/api/v10/channels/{channel}/messages"))
        .map(|r| serde_json::from_slice(&r.body).expect("outbound JSON"))
        .collect()
}

fn assert_safe_reminder(requests: &[RestRequest]) {
    let wire = posts(requests, CHANNEL);
    assert_eq!(
        wire.len(),
        1,
        "due reminder must be attempted despite the schedule failure"
    );
    let body = &wire[0];
    let content = body["content"].as_str().expect("reminder body");
    assert!(content.contains(&format!("<@{USER}>")));
    assert!(!content.contains(DECLINED));
    assert!(!two_bot_core::message_safety::contains_mass_mention(
        content
    ));
    assert!(two_bot_core::message_safety::text_len(content) <= 2_000);
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    assert!(
        requests.iter().all(|r| {
            r.path.starts_with("/api/v10/channels/4444/messages")
                || r.path == format!("/api/v10/channels/{CHANNEL}/messages")
        }),
        "no DMs, other channels or recipient lookups"
    );
}

#[derive(Clone, Copy)]
enum Fault {
    Claim,
    Arm,
    Retry,
    PermanentCompletion,
    StopAfterCleanup,
}

async fn fault(pool: &PgPool, mode: Fault) {
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_phase_schedule() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF (TG_ARGV[0]='claim' AND OLD.claim_token IS NULL AND NEW.claim_token IS NOT NULL)
           OR (TG_ARGV[0]='arm' AND NOT OLD.delivery_uncertain AND NEW.delivery_uncertain)
           OR (TG_ARGV[0]='retry' AND OLD.claim_token IS NOT NULL AND NEW.claim_token IS NULL
               AND NEW.last_run_at IS NOT DISTINCT FROM OLD.last_run_at)
           OR (TG_ARGV[0]='completion' AND NEW.last_run_at IS DISTINCT FROM OLD.last_run_at)
           THEN RAISE EXCEPTION 'private-phase-store-error-sentinel'; END IF;
           RETURN NEW;
         END $$;",
    )
    .execute(pool)
    .await
    .expect("install schedule-only failure function");
    let trigger = match mode {
        Fault::Claim => "CREATE TRIGGER fail_phase_schedule BEFORE UPDATE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION refuse_phase_schedule('claim')",
        Fault::Arm => "CREATE TRIGGER fail_phase_schedule BEFORE UPDATE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION refuse_phase_schedule('arm')",
        Fault::Retry => "CREATE TRIGGER fail_phase_schedule BEFORE UPDATE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION refuse_phase_schedule('retry')",
        Fault::PermanentCompletion | Fault::StopAfterCleanup => "CREATE TRIGGER fail_phase_schedule BEFORE UPDATE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION refuse_phase_schedule('completion')",
    };
    sqlx::raw_sql(trigger)
        .execute(pool)
        .await
        .expect("refuse only the selected schedule write");
}

async fn refuse_audit(pool: &PgPool, reminders: bool) {
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_phase_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.action=TG_ARGV[0] THEN RAISE EXCEPTION 'private-phase-audit-error-sentinel'; END IF;
         RETURN NEW; END $$;",
    ).execute(pool).await.expect("install scoped audit fault");
    let trigger = if reminders {
        "CREATE TRIGGER fail_phase_audit BEFORE INSERT ON automation_audit_log FOR EACH ROW EXECUTE FUNCTION refuse_phase_audit('rsvp.reminder')"
    } else {
        "CREATE TRIGGER fail_phase_audit BEFORE INSERT ON automation_audit_log FOR EACH ROW EXECUTE FUNCTION refuse_phase_audit('scheduled.run')"
    };
    sqlx::raw_sql(trigger)
        .execute(pool)
        .await
        .expect("refuse only one phase's result audit");
}

// Each case names a different early return, not a mocked phase implementation.
async fn completed_schedule_failure(mode: Fault) {
    let Some(db) = fixture().await else { return };
    schedule(db.pool(), "first", NOW - 1_000).await;
    schedule(db.pool(), "unvisited", NOW - 500).await;
    event(db.pool(), GUILD, EVENT, "scheduled", Some(CHANNEL)).await;
    fault(db.pool(), mode).await;
    let response = match mode {
        Fault::Retry => ScriptedResponse::status(503),
        Fault::PermanentCompletion => ScriptedResponse::status(403),
        _ => posted(),
    };
    let mock = rest(response, posted()).await;
    let capture = crate::tracing_capture::global();
    let start = capture.text().len();
    let case = next_capture_case(match mode {
        Fault::Claim => "claim",
        Fault::Arm => "arm",
        Fault::Retry => "retry",
        Fault::PermanentCompletion => "completion",
        Fault::StopAfterCleanup => "stop-after-cleanup",
    });
    let result = run_attributed(db.pool(), &executor(&mock), NOW, &case).await;
    let first = get_scheduled(db.pool(), GUILD, "first")
        .await
        .expect("read first")
        .expect("first exists");
    let unvisited = get_scheduled(db.pool(), GUILD, "unvisited")
        .await
        .expect("read unvisited")
        .expect("unvisited exists");
    let before = facts(db.pool(), EVENT).await;
    let requests = mock.requests();
    let log = attributed(capture, start, &case);
    // Restore the schedule store, then use an independent pool and executor:
    // no cached error, executor guard or lease can stand in for the fence.
    sqlx::raw_sql("DROP TRIGGER fail_phase_schedule ON scheduled_messages")
        .execute(db.pool())
        .await
        .expect("remove fixture schedule fault");
    let peer = db.independent_pool().await.expect("restart observer");
    let restart = run(&peer, &executor(&mock), NOW + 1).await;
    let after = facts(&peer, EVENT).await;
    let after_requests = mock.requests();
    peer.close().await;
    mock.shutdown().await;
    db.close()
        .await
        .expect("verified teardown before RED assertions");

    assert_eq!(
        result,
        Err(ErrorClass::Database),
        "reminder success must not hide the schedule error"
    );
    assert_safe_reminder(&requests);
    assert_fenced(&before, EVENT, Some("posted"));
    assert_eq!(restart, Ok(()));
    assert_eq!(
        before, after,
        "restart must not clear or rewrite the one-attempt evidence"
    );
    assert_eq!(posts(&after_requests, CHANNEL).len(), 1);
    assert!(unvisited.enabled);
    assert_eq!(
        unvisited.claim_token, None,
        "do not resume the failed scheduled batch"
    );
    assert_eq!(unvisited.last_run_at, None);
    assert_eq!(
        first.last_run_at, None,
        "a failed completion was never committed"
    );
    assert_eq!(first.last_message_id, None);
    assert_eq!(
        posts(&requests, SCHEDULE_CHANNEL).len(),
        match mode {
            Fault::Claim | Fault::Arm => 0,
            _ => 1,
        }
    );
    assert!(
        !log.is_empty(),
        "capture must attribute some diagnostics to the exercised tick"
    );
    for sensitive in [
        PRIVATE_BODY,
        PRIVATE_NAME,
        USER,
        DECLINED,
        "private-phase-store-error-sentinel",
    ] {
        assert!(
            !log.contains(sensitive),
            "phase diagnostics must be content-free"
        );
    }
}

#[tokio::test]
async fn claim_error_still_attempts_due_reminder_and_reports_database() {
    completed_schedule_failure(Fault::Claim).await;
}

#[tokio::test]
async fn occurrence_fence_error_still_attempts_due_reminder_and_reports_database() {
    completed_schedule_failure(Fault::Arm).await;
}

#[tokio::test]
async fn occurrence_retry_error_still_attempts_due_reminder_and_reports_database() {
    completed_schedule_failure(Fault::Retry).await;
}

#[tokio::test]
async fn permanent_refusal_recording_error_still_attempts_due_reminder_and_reports_database() {
    completed_schedule_failure(Fault::PermanentCompletion).await;
}

#[tokio::test]
async fn completion_stop_after_confirmed_cleanup_still_attempts_due_reminder() {
    completed_schedule_failure(Fault::StopAfterCleanup).await;
}

// An audit failure is already deferred on the baseline. Keep that control, and
// reject an implementation that rolls back the earlier completed schedule.
#[tokio::test]
async fn scheduled_audit_failure_keeps_completion_and_attempts_reminder() {
    let Some(db) = fixture().await else { return };
    schedule(db.pool(), "committed", NOW - 1_000).await;
    event(db.pool(), GUILD, EVENT, "scheduled", Some(CHANNEL)).await;
    refuse_audit(db.pool(), false).await;
    let mock = rest(posted(), posted()).await;
    let result = run(db.pool(), &executor(&mock), NOW).await;
    let row = get_scheduled(db.pool(), GUILD, "committed")
        .await
        .expect("read schedule")
        .expect("schedule exists");
    let reminder = facts(db.pool(), EVENT).await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
    assert_eq!(result, Err(ErrorClass::Database));
    assert_committed(&row);
    assert_safe_reminder(&requests);
    assert_fenced(&reminder, EVENT, Some("posted"));
}

fn assert_committed(row: &ScheduledMessageRow) {
    assert!(!row.enabled, "successful one-shot stays completed");
    assert_eq!(
        row.last_run_at.as_deref(),
        Some(format_iso_ms(NOW).as_str())
    );
    assert_eq!(row.last_message_id.as_deref(), Some("9001"));
    assert_eq!(row.claim_token, None);
}

// A later reminder-store failure must not undo an earlier scheduled message,
// fabricate a result or allow another send after the failed receipt write.
#[tokio::test]
async fn reminder_result_audit_failure_keeps_schedule_and_pre_wire_fence() {
    let Some(db) = fixture().await else { return };
    schedule(db.pool(), "committed", NOW - 1_000).await;
    event(db.pool(), GUILD, EVENT, "scheduled", Some(CHANNEL)).await;
    refuse_audit(db.pool(), true).await;
    let mock = rest(posted(), posted()).await;
    let result = run(db.pool(), &executor(&mock), NOW).await;
    let row = get_scheduled(db.pool(), GUILD, "committed")
        .await
        .expect("read schedule")
        .expect("schedule exists");
    let before = facts(db.pool(), EVENT).await;
    sqlx::raw_sql("DROP TRIGGER fail_phase_audit ON automation_audit_log")
        .execute(db.pool())
        .await
        .expect("restore result auditing");
    let peer = db.independent_pool().await.expect("restart pool");
    let restart = run(&peer, &executor(&mock), NOW + 1).await;
    let after = facts(&peer, EVENT).await;
    let requests = mock.requests();
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
    assert_eq!(result, Err(ErrorClass::Database));
    assert_committed(&row);
    assert_safe_reminder(&requests);
    assert_fenced(&before, EVENT, None);
    assert_eq!(restart, Ok(()));
    assert_eq!(before, after);
}

// A recorded optional delivery refusal is not a phase-store failure or proof
// of delivery. Preserve that best-effort contract and the immutable fence.
#[tokio::test]
async fn recorded_reminder_transport_failures_never_recycle_the_fence() {
    for (response, outcome) in [
        (ScriptedResponse::status(403), "failed"),
        (ScriptedResponse::status(503), "delivery_uncertain"),
        (ScriptedResponse::json(200, json!({})), "delivery_uncertain"),
    ] {
        let Some(db) = fixture().await else { return };
        schedule(db.pool(), "committed", NOW - 1_000).await;
        event(db.pool(), GUILD, EVENT, "scheduled", Some(CHANNEL)).await;
        let mock = rest(posted(), response).await;
        let result = run(db.pool(), &executor(&mock), NOW).await;
        let row = get_scheduled(db.pool(), GUILD, "committed")
            .await
            .expect("read schedule")
            .expect("schedule exists");
        let before = facts(db.pool(), EVENT).await;
        let peer = db.independent_pool().await.expect("restart pool");
        let restart = run(&peer, &executor(&mock), NOW + 1).await;
        let after = facts(&peer, EVENT).await;
        let requests = mock.requests();
        peer.close().await;
        mock.shutdown().await;
        db.close().await.expect("verified teardown");
        assert_committed(&row);
        assert_safe_reminder(&requests);
        assert_fenced(&before, EVENT, Some(outcome));
        assert_eq!(before, after);
        assert_eq!(restart, Ok(()));
        assert_eq!(
            result,
            Ok(()),
            "the optional delivery failure was recorded, not swallowed"
        );
    }
}

#[tokio::test]
async fn recorded_local_guard_refusal_cannot_be_retried_by_a_healthy_executor() {
    let Some(db) = fixture().await else { return };
    event(db.pool(), GUILD, EVENT, "scheduled", Some(CHANNEL)).await;
    let mock = rest(posted(), posted()).await;
    crate::gateway::ensure_crypto_provider();
    let guard = Arc::new(RateLimitGuard::new(Default::default()).expect("isolated guard"));
    guard.observe_status(401, true);
    let rest = ActionExecutor::with_proxy_and_guard(
        "synthetic-phase-isolation-token".to_owned(),
        Some(mock.origin()),
        guard,
    )
    .expect("latched local transport guard");
    let result = run(db.pool(), &rest, NOW).await;
    let before = facts(db.pool(), EVENT).await;
    let restart = run(db.pool(), &executor(&mock), NOW + 1).await;
    let after = facts(db.pool(), EVENT).await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
    assert!(
        requests.is_empty(),
        "a local refusal must never reach Discord"
    );
    assert_fenced(&before, EVENT, Some("failed"));
    assert_eq!(before, after);
    assert_eq!(restart, Ok(()));
    assert_eq!(
        result,
        Ok(()),
        "the optional refusal has a durable failed result"
    );
}

#[tokio::test]
async fn render_refusal_never_posts_or_recycles_its_attempt() {
    let Some(db) = fixture().await else { return };
    event(
        db.pool(),
        GUILD,
        "invalid-event-id",
        "scheduled",
        Some(CHANNEL),
    )
    .await;
    let mock = rest(posted(), posted()).await;
    let result = run(db.pool(), &executor(&mock), NOW).await;
    let before = facts(db.pool(), "invalid-event-id").await;
    let restart = run(db.pool(), &executor(&mock), NOW + 1).await;
    let after = facts(db.pool(), "invalid-event-id").await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
    assert!(requests.is_empty());
    assert_fenced(&before, "invalid-event-id", Some("failed"));
    assert_eq!(before, after);
    assert_eq!(restart, Ok(()));
    assert_eq!(
        result,
        Ok(()),
        "a recorded render refusal is not proof of delivery"
    );
}

// Both errors use the existing Database class. Observing the reminder fence
// proves that returning the first error did not silently skip the second phase.
#[tokio::test]
async fn combined_store_failures_have_stable_classification_and_no_false_receipt() {
    let Some(db) = fixture().await else { return };
    schedule(db.pool(), "first", NOW - 1_000).await;
    event(db.pool(), GUILD, EVENT, "scheduled", Some(CHANNEL)).await;
    fault(db.pool(), Fault::Claim).await;
    refuse_audit(db.pool(), true).await;
    let mock = rest(posted(), posted()).await;
    let capture = crate::tracing_capture::global();
    let log_start = capture.text().len();
    let case = next_capture_case("combined");
    let first = run_attributed(db.pool(), &executor(&mock), NOW, &case).await;
    let diagnostics = attributed(capture, log_start, &case).to_lowercase();
    let before = facts(db.pool(), EVENT).await;
    let next = run(db.pool(), &executor(&mock), NOW + 1).await;
    let after = facts(db.pool(), EVENT).await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
    assert_eq!(first, Err(ErrorClass::Database));
    assert_eq!(next, Err(ErrorClass::Database));
    assert_safe_reminder(&requests);
    assert_fenced(&before, EVENT, None);
    assert_eq!(before, after);
    // Accept separate records or one combined classification, without pinning
    // the exact message, field layout, function names or phase-result type.
    for phase in ["scheduled", "reminder"] {
        assert!(
            diagnostics
                .lines()
                .any(|line| line.contains(phase) && line.contains("database")),
            "both failed phases need content-free classified diagnostics"
        );
    }
    for sensitive in [
        PRIVATE_NAME,
        PRIVATE_BODY,
        USER,
        DECLINED,
        "private-phase-store-error-sentinel",
        "private-phase-audit-error-sentinel",
    ] {
        assert!(!diagnostics.contains(&sensitive.to_lowercase()));
    }
}

#[tokio::test]
async fn failed_schedule_phase_still_limits_reminders_to_two_events_per_tick() {
    let Some(db) = fixture().await else { return };
    schedule(db.pool(), "first", NOW - 1_000).await;
    fault(db.pool(), Fault::Claim).await;
    for id in [EVENT, EVENT2, EVENT3] {
        event(db.pool(), GUILD, id, "scheduled", Some(CHANNEL)).await;
    }
    let mock = rest(posted(), posted()).await;
    let first = run(db.pool(), &executor(&mock), NOW).await;
    let first_requests = mock.requests();
    let third_before = facts(db.pool(), EVENT3).await;
    let second = run(db.pool(), &executor(&mock), NOW + 1).await;
    let second_requests = mock.requests();
    let third_after = facts(db.pool(), EVENT3).await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
    assert_eq!(first, Err(ErrorClass::Database));
    assert_eq!(second, Err(ErrorClass::Database));
    assert_eq!(
        posts(&first_requests, CHANNEL).len(),
        2,
        "bounded reminder work, not the whole due set"
    );
    assert!(third_before.is_empty());
    assert_eq!(
        posts(&second_requests, CHANNEL).len(),
        3,
        "already-fenced events must not starve the next event"
    );
    assert_fenced(&third_after, EVENT3, Some("posted"));
}

#[tokio::test]
async fn failed_schedule_does_not_widen_guild_status_channel_or_recipient_authorization() {
    let Some(db) = fixture().await else { return };
    schedule(db.pool(), "first", NOW - 1_000).await;
    fault(db.pool(), Fault::Claim).await;
    event(db.pool(), GUILD, EVENT, "scheduled", Some(CHANNEL)).await;
    event(db.pool(), "7777", EVENT, "scheduled", Some(CHANNEL)).await;
    event(db.pool(), GUILD, EVENT2, "cancelled", Some(CHANNEL)).await;
    event(db.pool(), GUILD, EVENT3, "scheduled", None).await;
    let mock = rest(posted(), posted()).await;
    let result = run(db.pool(), &executor(&mock), NOW).await;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM automation_audit_log WHERE action='rsvp.reminder_attempt'",
    )
    .fetch_one(db.pool())
    .await
    .expect("count all fixture admissions");
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
    assert_eq!(result, Err(ErrorClass::Database));
    assert_safe_reminder(&requests);
    assert_eq!(
        count, 1,
        "only the authorized guild's eligible event is admitted"
    );
}

fn supervised_action(pool: PgPool, rest: ActionExecutor) -> JobAction {
    Arc::new(move || {
        let pool = pool.clone();
        let rest = rest.clone();
        Box::pin(async move { tick(&pool, &rest, GUILD, || NOW).await })
    })
}

fn enabled() -> FeatureGates {
    FeatureGates::from_map(&std::collections::HashMap::from([(
        "TWO_AUTOMATIONS".to_owned(),
        "1".to_owned(),
    )]))
    .expect("valid feature gate")
}

async fn settle() {
    for _ in 0..40 {
        tokio::task::yield_now().await;
    }
}

// Cancellation is not a completed phase error: shutdown owns and joins the
// entire tick, including when a Discord receipt is blocked. No detached tail.
#[tokio::test]
async fn supervisor_shutdown_during_schedule_prevents_any_late_reminder_tail() {
    let Some(db) = fixture().await else { return };
    schedule(db.pool(), "held", NOW - 1_000).await;
    event(db.pool(), GUILD, EVENT, "scheduled", Some(CHANNEL)).await;
    let (mock, gate) = MockRest::start_gated(vec![posted()], posted()).await;
    let mut job = super::super::register_fenced(
        enabled(),
        &fixtures::staging(),
        supervised_action(db.pool().clone(), executor(&mock)),
    )
    .expect("authorized scheduler job");
    job.startup_jitter = Duration::ZERO;
    job.cadence = Duration::from_secs(3_600);
    let status = jobs::statuses(&["scheduled_messages"], false);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(jobs::supervise(vec![job], status.clone(), rx));
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("scheduled POST is in flight");
    let before = facts(db.pool(), EVENT).await;
    stop.send(true).expect("revoke supervisor ownership");
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("shutdown joins its action")
        .expect("supervisor task")
        .expect("registered job");
    gate.release();
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_completion())
        .await
        .expect("late schedule response completed");
    settle().await;
    let after = facts(db.pool(), EVENT).await;
    let requests = mock.requests();
    let current = status.read().await["scheduled_messages"].clone();
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
    assert!(before.is_empty());
    assert!(
        after.is_empty(),
        "ownership loss must not start an independent reminder action"
    );
    assert_eq!(posts(&requests, CHANNEL).len(), 0);
    assert_eq!(posts(&requests, SCHEDULE_CHANNEL).len(), 1);
    assert!(!current.running);
    assert_eq!(current.last_success, None);
}

#[tokio::test]
async fn shared_supervisor_deadline_is_not_renewed_for_a_blocked_reminder_phase() {
    let Some(db) = fixture().await else { return };
    schedule(db.pool(), "committed", NOW - 1_000).await;
    event(db.pool(), GUILD, EVENT, "scheduled", Some(CHANNEL)).await;
    event(db.pool(), GUILD, EVENT2, "scheduled", Some(CHANNEL)).await;
    let (mock, gate) = MockRest::start_gated(
        vec![posted().delayed(Duration::from_secs(2)), posted()],
        posted(),
    )
    .await;
    let mut job = super::super::register_fenced(
        enabled(),
        &fixtures::staging(),
        supervised_action(db.pool().clone(), executor(&mock)),
    )
    .expect("authorized scheduler job");
    // A shorter test budget exercises the real supervisor timeout without a
    // two-minute sleep. The normal registered job's timeout is not changed.
    job.timeout = Duration::from_secs(6);
    job.startup_jitter = Duration::ZERO;
    job.cadence = Duration::from_secs(3_600);
    let status = jobs::statuses(&["scheduled_messages"], false);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(jobs::supervise(vec![job], status.clone(), rx));
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("reminder POST reached after schedule work consumed budget");
    let before = facts(db.pool(), EVENT).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(4)).await;
    settle().await;
    let current = status.read().await["scheduled_messages"].clone();
    tokio::time::resume();
    stop.send(true).expect("stop supervisor");
    task.await
        .expect("supervisor task")
        .expect("registered job");
    gate.release();
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_completion())
        .await
        .expect("late reminder response finished");
    settle().await;
    let after = facts(db.pool(), EVENT).await;
    let second = facts(db.pool(), EVENT2).await;
    let row = get_scheduled(db.pool(), GUILD, "committed")
        .await
        .expect("read schedule")
        .expect("schedule exists");
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
    assert_eq!(
        current.last_error_class,
        Some(ErrorClass::Timeout),
        "one overall deadline covers both phases"
    );
    assert!(!current.running);
    assert_eq!(current.last_success, None);
    assert_committed(&row);
    assert_fenced(&before, EVENT, None);
    assert_eq!(
        before, after,
        "late receipt cannot create a detached result"
    );
    assert!(
        second.is_empty(),
        "exhausted shared budget must not admit the next event"
    );
    assert_eq!(posts(&requests, CHANNEL).len(), 1);
}
